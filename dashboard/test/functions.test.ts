import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { onRequest as middleware } from "../functions/_middleware.ts";
import * as kill from "../functions/api/kill.ts";
import * as push from "../functions/api/push.ts";
import * as status from "../functions/api/status.ts";
import * as summary from "../functions/api/summary.ts";
import { safeEqual } from "../src/auth.ts";
import { isPreviewBot } from "../src/share.ts";
import { KEYS, LIMITS, type StatusRecord } from "../src/model.ts";
import { FakeKV, PASSWORD, TOKEN, basic, bearer, call, fixtureText, makeEnv } from "./fakes.ts";

const fixture = fixtureText;
const U = "https://susquatch.pages.dev";

describe("middleware", () => {
  const env = makeEnv();
  const page = async () => new Response("<html>");

  it("challenges anonymous requests, including static pages", async () => {
    const res = await call(page, { url: `${U}/` }, env, { middleware });
    assert.equal(res.status, 401);
    assert.match(res.headers.get("WWW-Authenticate") ?? "", /^Basic realm="susquatch"/);
    assert.equal(res.headers.get("X-Robots-Tag"), "noindex, nofollow, noarchive");
  });

  it("rejects a wrong password and a wrong token", async () => {
    assert.equal((await call(page, { url: `${U}/`, headers: { Authorization: basic("nope") } }, env, { middleware })).status, 401);
    const res = await call(page, { url: `${U}/api/push`, method: "POST", headers: { Authorization: bearer("x".repeat(40)) } }, env, { middleware });
    assert.equal(res.status, 401);
  });

  it("lets the human in with security headers and no caching", async () => {
    const res = await call(page, { url: `${U}/`, headers: { Authorization: basic() } }, env, { middleware });
    assert.equal(res.status, 200);
    assert.match(res.headers.get("Content-Security-Policy") ?? "", /frame-ancestors 'none'/);
    assert.equal(res.headers.get("Cache-Control"), "no-store");
    assert.equal(res.headers.get("X-Frame-Options"), "DENY");
  });

  it("confines the server token to the machine routes", async () => {
    const ui = await call(page, { url: `${U}/`, headers: { Authorization: bearer() } }, env, { middleware });
    assert.equal(ui.status, 403);
    const pressKill = await call(page, { url: `${U}/api/kill`, method: "POST", headers: { Authorization: bearer() } }, env, { middleware });
    assert.equal(pressKill.status, 403);
    const poll = await call(page, { url: `${U}/api/kill`, headers: { Authorization: bearer() } }, env, { middleware });
    assert.equal(poll.status, 200);
  });

  it("serves robots.txt without credentials", async () => {
    const res = await call(async () => new Response("User-agent: *\nDisallow: /"), { url: `${U}/robots.txt` }, env, { middleware });
    assert.equal(res.status, 200);
  });

  describe("link previews", () => {
    const LI = { "User-Agent": "LinkedInBot/1.0 (compatible; Mozilla/5.0; Apache-HttpClient +http://www.linkedin.com)", Accept: "*/*" };

    it("sends a preview crawler from / to the public login page, never the dashboard", async () => {
      const res = await call(page, { url: `${U}/`, headers: LI }, env, { middleware });
      assert.equal(res.status, 302);
      assert.equal(res.headers.get("Location"), `${U}/login`);
    });

    it("serves the login page and share card to a crawler without noindex", async () => {
      for (const path of ["/login", "/og.png"]) {
        const res = await call(page, { url: `${U}${path}`, headers: LI }, env, { middleware });
        assert.equal(res.status, 200, path);
        assert.equal(res.headers.get("X-Robots-Tag"), null, path);
      }
    });

    it("keeps noindex on the share pages for everyone else", async () => {
      const res = await call(page, { url: `${U}/login` }, env, { middleware });
      assert.equal(res.headers.get("X-Robots-Tag"), "noindex, nofollow, noarchive");
    });

    it("still refuses a crawler anything private", async () => {
      for (const path of ["/api/status", "/app.js", "/index.html"]) {
        const res = await call(page, { url: `${U}${path}`, headers: LI }, env, { middleware });
        assert.notEqual(res.status, 200, path);
        assert.equal(res.headers.get("X-Robots-Tag"), "noindex, nofollow, noarchive", path);
      }
    });

    it("recognizes the common unfurlers and not ordinary browsers", () => {
      const ua = (s: string) => new Request(U, { headers: { "User-Agent": s } });
      for (const s of ["Twitterbot/1.0", "facebookexternalhit/1.1", "Slackbot-LinkExpanding 1.0", "Discordbot/2.0", "WhatsApp/2.23"]) {
        assert.equal(isPreviewBot(ua(s)), true, s);
      }
      assert.equal(isPreviewBot(ua("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/130.0")), false);
    });
  });

  it("fails closed when secrets are missing or weak", async () => {
    const weak = makeEnv(new FakeKV(), { DASHBOARD_PASSWORD: "short", SUSQ_STATUS_PUSH_TOKEN: "" });
    const res = await call(page, { url: `${U}/`, headers: { Authorization: basic("short") } }, weak, { middleware });
    assert.equal(res.status, 503);
    const noPw = makeEnv(new FakeKV(), { DASHBOARD_PASSWORD: undefined });
    const res2 = await call(page, { url: `${U}/`, headers: { Authorization: basic("") } }, noPw, { middleware });
    assert.equal(res2.status, 401, "an empty password never matches an unset secret");
  });

  it("compares secrets in constant time but correctly", async () => {
    assert.equal(await safeEqual(PASSWORD, PASSWORD), true);
    assert.equal(await safeEqual(PASSWORD, `${PASSWORD}x`), false);
    assert.equal(await safeEqual("", TOKEN), false);
  });
});

describe("POST /api/push", () => {
  const post = (env: ReturnType<typeof makeEnv>, body: string, principal: "server" | "human" = "server") =>
    call(push.onRequestPost, { method: "POST", body, headers: { "Content-Type": "application/json" } }, env, { principal });

  it("stores the snapshot with exactly one KV write", async () => {
    const env = makeEnv();
    const res = await post(env, fixture);
    assert.equal(res.status, 200);
    assert.equal(env.SUSQ_KV.writes, 1);
    const rec = env.SUSQ_KV.json<StatusRecord>(KEYS.status)!;
    assert.equal(rec.snapshot.equity, 100431.5);
    assert.equal(rec.history.length, 1);
    assert.equal(rec.events[0]?.text, "first qualifying trade filled");
  });

  it("rate-limits faster-than-budget pushes without writing", async () => {
    const env = makeEnv();
    await post(env, fixture);
    const res = await post(env, fixture);
    assert.equal(res.status, 429);
    assert.ok(Number(res.headers.get("Retry-After")) > 0);
    assert.equal(env.SUSQ_KV.writes, 1);
  });

  it("rejects humans, bad JSON, invalid shapes and oversize bodies", async () => {
    const env = makeEnv();
    assert.equal((await post(env, fixture, "human")).status, 403);
    assert.equal((await post(env, "{not json")).status, 400);
    assert.equal((await post(env, JSON.stringify({ mode: "paper" }))).status, 400);
    assert.equal((await post(env, " ".repeat(LIMITS.bodyBytes + 1))).status, 413);
    assert.equal(env.SUSQ_KV.writes, 0);
  });
});

describe("/api/kill", () => {
  const press = (env: ReturnType<typeof makeEnv>, body: unknown, headers: Record<string, string> = {}) =>
    call(
      kill.onRequestPost,
      {
        url: `${U}/api/kill`,
        method: "POST",
        body: JSON.stringify(body),
        headers: { "Content-Type": "application/json", "X-Susq-Intent": "kill", Origin: U, ...headers },
      },
      env,
      { principal: "human" },
    );

  it("GET is not-killed by default", async () => {
    const res = await call(kill.onRequestGet, {}, makeEnv(), { principal: "server" });
    assert.deepEqual(await res.json(), { killed: false });
  });

  it("the confirmed second step sets the flag once; the poller sees it", async () => {
    const env = makeEnv();
    const res = await press(env, { confirm: "kill", reason: "needle looks wrong" });
    assert.equal(res.status, 201);
    const again = await press(env, { confirm: "kill", reason: "second press" });
    assert.equal(((await again.json()) as { reason: string }).reason, "needle looks wrong");
    assert.equal(env.SUSQ_KV.writes, 1);
    const polled = (await (await call(kill.onRequestGet, {}, env, { principal: "server" })).json()) as { killed: boolean };
    assert.equal(polled.killed, true);
  });

  it("refuses without the confirm step, intent header, JSON, or from another origin", async () => {
    const env = makeEnv();
    assert.equal((await press(env, { reason: "x" })).status, 400);
    assert.equal((await press(env, { confirm: "kill", reason: "x" }, { "X-Susq-Intent": "" })).status, 400);
    assert.equal((await press(env, { confirm: "kill", reason: "x" }, { "Content-Type": "text/plain" })).status, 415);
    assert.equal((await press(env, { confirm: "kill", reason: "x" }, { Origin: "https://evil.example" })).status, 403);
    assert.equal(env.SUSQ_KV.writes, 0);
  });

  it("only the server (make unkill) clears it; the browser can't", async () => {
    const env = makeEnv();
    await press(env, { confirm: "kill", reason: "x" });
    assert.equal((await call(kill.onRequestDelete, { method: "DELETE" }, env, { principal: "human" })).status, 403);
    assert.equal((await call(kill.onRequestDelete, { method: "DELETE" }, env, { principal: "server" })).status, 200);
    assert.equal(env.SUSQ_KV.json(KEYS.kill), null);
  });
});

describe("GET /api/status and /api/summary", () => {
  it("reports staleness from the last receive time", async () => {
    const env = makeEnv();
    const rec: StatusRecord = { v: 1, receivedAt: Date.now() - 3_600_000, snapshot: {}, history: [], events: [] };
    await env.SUSQ_KV.put(KEYS.status, JSON.stringify(rec));
    const body = (await (await call(status.onRequestGet, {}, env, { principal: "human" })).json()) as {
      stale: boolean;
      heartbeatAgeMs: number;
      kill: { killed: boolean };
    };
    assert.equal(body.stale, true);
    assert.ok(body.heartbeatAgeMs >= 3_600_000);
    assert.equal(body.kill.killed, false);
  });

  it("is stale (not an error) when nothing was ever pushed", async () => {
    const body = (await (await call(status.onRequestGet, {}, makeEnv(), { principal: "human" })).json()) as {
      stale: boolean;
      status: unknown;
    };
    assert.equal(body.stale, true);
    assert.equal(body.status, null);
  });

  it("is human-only", async () => {
    assert.equal((await call(status.onRequestGet, {}, makeEnv(), { principal: "server" })).status, 403);
    assert.equal((await call(summary.onRequestGet, {}, makeEnv(), { principal: "server" })).status, 403);
  });
});
