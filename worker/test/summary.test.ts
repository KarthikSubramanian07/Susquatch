import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { type StatusRecord, applyPush, parseSnapshot } from "../../dashboard/src/model.ts";
import { change24h, compose, duration } from "../src/summary.ts";
import { fixtureText } from "../../dashboard/test/fakes.ts";

const fixture = JSON.parse(fixtureText);
const NOW = new Date("2026-10-06T14:00:00Z");
const T = NOW.getTime();
const MIN = 60_000;
const opts = { now: NOW, deadmanMinutes: 20, dashboardUrl: "https://susquatch.pages.dev" };

function record(overrides: Record<string, unknown> = {}, receivedAt = T - 2 * MIN): StatusRecord {
  const snap = parseSnapshot({ ...fixture, events: [], ...overrides });
  const rec = applyPush(null, snap, receivedAt);
  return rec;
}

describe("dead-man", () => {
  it("a fresh heartbeat is not silent", () => {
    const s = compose(record(), null, opts);
    assert.equal(s.silent, null);
    assert.deepEqual(s.actionNeeded, []);
    assert.doesNotMatch(s.markdown, /silent/i);
  });

  it("puts 'server silent since X' first, above ACTION NEEDED, when the heartbeat is old", () => {
    const s = compose(record({}, T - (10 * 60 + 48) * MIN), null, opts);
    assert.equal(s.silent?.since, new Date(T - (10 * 60 + 48) * MIN).toISOString());
    assert.match(s.actionNeeded[0] ?? "", /^Server silent since 2026-10-06 03:12 UTC \(10h 48m\)/);
    const lines = s.markdown.split("\n");
    const silentAt = lines.findIndex((l) => l.includes("Server silent since"));
    const actionAt = lines.findIndex((l) => l === "**ACTION NEEDED**");
    assert.ok(silentAt > 0 && silentAt < actionAt, "silent banner comes before ACTION NEEDED");
  });

  it("threshold is exclusive and configurable", () => {
    assert.equal(compose(record({}, T - 20 * MIN), null, opts).silent, null);
    assert.notEqual(compose(record({}, T - 21 * MIN), null, opts).silent, null);
    assert.equal(compose(record({}, T - 21 * MIN), null, { ...opts, deadmanMinutes: 30 }).silent, null);
  });

  it("never-pushed is its own, explicit case", () => {
    const s = compose(null, null, opts);
    assert.deepEqual(s.silent, { since: "never", minutes: -1 });
    assert.match(s.actionNeeded[0] ?? "", /never pushed/);
    assert.match(s.markdown, /no status has ever been received/);
    assert.match(s.markdown, /\| n\/a \| n\/a \| n\/a \| n\/a \| unranked \|/);
  });
});

describe("ACTION NEEDED", () => {
  it("is empty on a good day and says so", () => {
    assert.match(compose(record(), null, opts).markdown, /\*\*ACTION NEEDED\*\*\n- nothing today/);
  });

  it("carries the server's items, deduplicated", () => {
    const s = compose(record({ actionNeeded: ["Rotate soon: github_token in 7d.", "Rotate soon: github_token in 7d."] }), null, opts);
    assert.deepEqual(s.actionNeeded, ["Rotate soon: github_token in 7d."]);
    assert.match(s.markdown, /- \[ \] Rotate soon/);
  });

  it("flags a dashboard Kill the server hasn't applied", () => {
    const s = compose(record(), { killed: true, at: "2026-10-06T13:55:00Z", reason: "x" }, opts);
    assert.match(s.actionNeeded[0] ?? "", /hasn't confirmed/);
    const applied = compose(record({ killed: true, killReason: "x" }), { killed: true, reason: "x" }, opts);
    assert.deepEqual(applied.actionNeeded, []);
    assert.match(applied.markdown, /\*\*kill\*\* ON \(x\)/);
  });
});

describe("content", () => {
  it("splits 24 h events into risk and notable, drops older ones", () => {
    const rec = record();
    rec.events = [
      { t: T - 30 * 60 * MIN, text: "old breaker tripped" },
      { t: T - 5 * 60 * MIN, text: "loss-per-hour breaker tripped: cancel-all + freeze" },
      { t: T - 4 * 60 * MIN, text: "first qualifying trade filled" },
      { t: T - 3 * 60 * MIN, text: "watchdog restarted unhealthy needle" },
    ];
    const s = compose(rec, null, opts);
    assert.deepEqual(s.risk, ["09:00Z loss-per-hour breaker tripped: cancel-all + freeze", "11:00Z watchdog restarted unhealthy needle"]);
    assert.deepEqual(s.notable, ["10:00Z first qualifying trade filled"]);
  });

  it("summarises money, rank, health and backups", () => {
    const rec = record({
      rank: 412,
      health: [
        { name: "platform", state: "ok" },
        { name: "backup", state: "degraded", detail: "last good backup 40h ago" },
      ],
    });
    rec.history = [
      { t: T - 25 * 60 * MIN, e: 99_000, d: 0, p: 0, r: 0 },
      { t: T - 23 * 60 * MIN, e: 99_500, d: 0, p: 0, r: 0 },
      ...rec.history,
    ];
    const s = compose(rec, null, opts);
    assert.equal(s.headline.change24h, 100431.5 - 99_000);
    assert.equal(s.health.ok, 1);
    assert.deepEqual(s.health.issues, ["backup: degraded (last good backup 40h ago)"]);
    assert.equal(s.backup, "degraded, last good backup 40h ago");
    assert.match(s.markdown, /\| 100,431\.50 \| \+1,431\.50 \| \+431\.50 \| \+431\.50 \| 412 \/ 2,150 \|/);
  });

  it("links the weekly review when there is one", () => {
    const s = compose(record(), null, { ...opts, weeklyReview: { title: "2026-W41", url: "https://github.com/x/y/blob/main/weekly/2026-W41.md" } });
    assert.match(s.markdown, /\*\*Weekly review\*\* \[2026-W41\]\(https:\/\/github\.com\/x\/y\/blob\/main\/weekly\/2026-W41\.md\)/);
  });

  it("change24h needs a sample at least a day old", () => {
    assert.equal(change24h(record(), T), null);
  });

  it("formats durations", () => {
    assert.equal(duration(5), "5m");
    assert.equal(duration(648), "10h 48m");
    assert.equal(duration(60 * 50), "2d 2h");
  });
});
