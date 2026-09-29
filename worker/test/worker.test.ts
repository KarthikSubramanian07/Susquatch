import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { KEYS, type Summary, applyPush, parseSnapshot } from "../../dashboard/src/model.ts";
import { FakeKV, fixtureText } from "../../dashboard/test/fakes.ts";
import worker, { type Env, run } from "../src/index.ts";

const fixture = JSON.parse(fixtureText);
const NOW = new Date("2026-10-06T14:00:00Z");

interface Call {
  method: string;
  url: string;
  body: unknown;
  auth: string | null;
}

/** A GitHub stand-in: records calls and answers the few endpoints the Worker uses. */
function fakeGitHub(opts: { existingIssue?: number; failComments?: number; weekly?: string[] } = {}) {
  const calls: Call[] = [];
  const impl = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url = String(input);
    const method = init?.method ?? "GET";
    const headers = new Headers(init?.headers);
    calls.push({ method, url, body: init?.body ? JSON.parse(String(init.body)) : undefined, auth: headers.get("Authorization") });
    const reply = (status: number, body: unknown) => new Response(JSON.stringify(body), { status });
    if (url.includes("/contents/weekly")) {
      return opts.weekly
        ? reply(200, opts.weekly.map((name) => ({ name, type: "file", html_url: `https://gh/weekly/${name}` })))
        : reply(404, {});
    }
    if (method === "GET" && url.includes("/issues?")) return reply(200, opts.existingIssue ? [{ number: opts.existingIssue }] : []);
    if (method === "POST" && url.endsWith("/issues")) return reply(201, { number: 7 });
    if (method === "POST" && url.includes("/comments")) {
      return opts.failComments ? reply(opts.failComments, {}) : reply(201, { html_url: `${url.replace("/repos/", "/")}#c1` });
    }
    return reply(404, {});
  };
  return { calls, impl: impl as typeof fetch };
}

function env(kv: FakeKV, overrides: Partial<Env> = {}): Env {
  return {
    SUSQ_KV: kv as unknown as KVNamespace,
    GITHUB_TOKEN: "github_pat_test",
    PUSH_TO_GITHUB: "true",
    REPORTS_REPO: "owner/susquatch-reports",
    DEADMAN_MINUTES: "20",
    ...overrides,
  };
}

function seeded(receivedAt = NOW.getTime() - 60_000): FakeKV {
  const kv = new FakeKV();
  kv.store.set(KEYS.status, JSON.stringify(applyPush(null, parseSnapshot(fixture), receivedAt)));
  return kv;
}

describe("scheduled run", () => {
  it("writes summary:latest + the dated key and comments on the existing issue", async () => {
    const kv = seeded();
    const gh = fakeGitHub({ existingIssue: 3, weekly: ["2026-W40.md", "2026-W41.md", "notes.txt"] });
    const s = await run(env(kv), NOW, gh.impl);

    assert.equal(kv.json<Summary>(KEYS.summaryLatest)?.date, "2026-10-06");
    assert.equal(kv.json<Summary>("summary:2026-10-06")?.posted?.url.endsWith("#c1"), true);
    assert.equal(kv.writes, 2);
    const comment = gh.calls.find((c) => c.url.endsWith("/issues/3/comments"));
    assert.ok(comment, "commented on issue #3");
    assert.equal(comment.auth, "Bearer github_pat_test");
    assert.match((comment.body as { body: string }).body, /### Susquatch · Tue, Oct 6, 2026|### Susquatch · Tue Oct 6, 2026/);
    assert.deepEqual(s.weeklyReview, { title: "2026-W41", url: "https://gh/weekly/2026-W41.md" });
  });

  it("creates the labelled issue when none is open", async () => {
    const gh = fakeGitHub();
    await run(env(seeded()), NOW, gh.impl);
    const created = gh.calls.find((c) => c.method === "POST" && c.url.endsWith("/issues"));
    assert.deepEqual((created?.body as { labels: string[] }).labels, ["daily-summary"]);
    assert.ok(gh.calls.some((c) => c.url.endsWith("/issues/7/comments")));
  });

  it("never posts twice for the same day", async () => {
    const kv = seeded();
    const gh = fakeGitHub({ existingIssue: 3 });
    await run(env(kv), NOW, gh.impl);
    await run(env(kv), new Date(NOW.getTime() + 60_000), gh.impl);
    assert.equal(gh.calls.filter((c) => c.url.includes("/comments")).length, 1);
    assert.ok(kv.json<Summary>(KEYS.summaryLatest)?.posted?.url);
  });

  it("push=false keeps it on the dashboard only", async () => {
    const kv = seeded();
    const gh = fakeGitHub({ existingIssue: 3 });
    const s = await run(env(kv, { PUSH_TO_GITHUB: "false" }), NOW, gh.impl);
    assert.equal(gh.calls.filter((c) => c.method === "POST").length, 0);
    assert.equal(s.posted, undefined);
    assert.ok(kv.json(KEYS.summaryLatest));
  });

  it("a GitHub failure becomes an ACTION NEEDED line on the dashboard, not a crash", async () => {
    const kv = seeded();
    const s = await run(env(kv), NOW, fakeGitHub({ existingIssue: 3, failComments: 401 }).impl);
    assert.match(s.actionNeeded.at(-1) ?? "", /Couldn't post today's summary to GitHub .*401/);
    assert.ok(kv.json<Summary>(KEYS.summaryLatest));
  });

  it("missing token with push on is reported", async () => {
    const s = await run(env(seeded(), { GITHUB_TOKEN: undefined }), NOW, fakeGitHub().impl);
    assert.match(s.actionNeeded.at(-1) ?? "", /GITHUB_TOKEN/);
  });

  it("dead server: still runs (on Cloudflare) and leads with the silence", async () => {
    const kv = seeded(NOW.getTime() - 5 * 3_600_000);
    const gh = fakeGitHub({ existingIssue: 3 });
    const s = await run(env(kv), NOW, gh.impl);
    assert.match(s.actionNeeded[0] ?? "", /^Server silent since 2026-10-06 09:00 UTC \(5h 00m\)/);
    const body = (gh.calls.find((c) => c.url.includes("/comments"))?.body as { body: string }).body;
    assert.ok(body.indexOf("Server silent since") < body.indexOf("ACTION NEEDED"));
  });

  it("the scheduled() entry point hands the run to waitUntil", async () => {
    const kv = seeded();
    const pending: Promise<unknown>[] = [];
    const ctx = { waitUntil: (p: Promise<unknown>) => pending.push(p), passThroughOnException() {}, props: {} };
    const controller = { scheduledTime: NOW.getTime(), cron: "0 14 * * *", noRetry() {} };
    await worker.scheduled(controller as never, env(kv, { PUSH_TO_GITHUB: "false" }), ctx as never);
    await Promise.all(pending);
    assert.equal(kv.json<Summary>(KEYS.summaryLatest)?.date, "2026-10-06");
  });
});
