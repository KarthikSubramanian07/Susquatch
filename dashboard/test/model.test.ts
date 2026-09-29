import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  LIMITS,
  ValidationError,
  applyPush,
  heartbeatAge,
  isStale,
  parseEvent,
  parseKill,
  parseSnapshot,
} from "../src/model.ts";
import { fixtureText } from "./fakes.ts";

const fixture = JSON.parse(fixtureText);

describe("parseSnapshot", () => {
  it("accepts the golden contract fixture (catches proto drift)", () => {
    const s = parseSnapshot(fixture);
    assert.equal(s.equity, 100431.5);
    assert.equal(s.fieldSize, 2150);
    assert.equal(s.positions?.[0]?.quantity, "-37");
    assert.equal(s.recentTrades?.[0]?.side, "SIDE_NO");
    assert.equal(s.header?.seq, "55");
  });

  it("drops unknown fields instead of failing (forward compatible)", () => {
    const s = parseSnapshot({ ...fixture, someFutureField: { x: 1 } }) as Record<string, unknown>;
    assert.equal(s.someFutureField, undefined);
  });

  for (const [name, patch] of [
    ["missing header", { header: undefined }],
    ["wrong schema version", { header: { ...fixture.header, schemaVersion: 2 } }],
    ["bad mode", { mode: "yolo" }],
    ["NaN-ish equity", { equity: "100" }],
    ["infinite equity", { equity: Number.POSITIVE_INFINITY }],
    ["non-list positions", { positions: {} }],
    ["bad int64", { positions: [{ quantity: "12abc" }] }],
    ["too many trades", { recentTrades: Array.from({ length: LIMITS.trades + 1 }, () => ({})) }],
  ] as const) {
    it(`rejects ${name}`, () => {
      assert.throws(() => parseSnapshot({ ...fixture, ...patch }), ValidationError);
    });
  }

  it("coerces unknown health states to down and truncates long text", () => {
    const s = parseSnapshot({ ...fixture, health: [{ name: "x", state: "weird", detail: "y".repeat(2000) }] });
    assert.equal(s.health?.[0]?.state, "down");
    assert.equal(s.health?.[0]?.detail?.length, LIMITS.text);
  });
});

describe("applyPush", () => {
  it("keeps latest snapshot, appends history and timestamped events, caps both", () => {
    const snap = parseSnapshot({ ...fixture, events: ["2026-10-05T03:12:00Z breaker tripped", "no stamp"] });
    let rec = applyPush(null, snap, 1_000);
    assert.equal(rec.receivedAt, 1_000);
    assert.deepEqual(rec.history, [{ t: 1_000, e: 100431.5, d: 431.5, p: 431.5, r: 0 }]);
    assert.deepEqual(rec.events, [
      { t: Date.parse("2026-10-05T03:12:00Z"), text: "breaker tripped" },
      { t: 1_000, text: "no stamp" },
    ]);
    assert.equal((rec.snapshot as Record<string, unknown>).events, undefined, "events live only in the ring");

    for (let i = 0; i < LIMITS.historyPoints + 10; i++) rec = applyPush(rec, snap, 2_000 + i);
    assert.equal(rec.history.length, LIMITS.historyPoints);
    assert.equal(rec.events.length, LIMITS.events);
    assert.equal(rec.history.at(-1)?.t, 2_000 + LIMITS.historyPoints + 9);
  });
});

describe("dead-man helpers", () => {
  it("measures heartbeat age and staleness", () => {
    const rec = applyPush(null, parseSnapshot(fixture), 10_000);
    assert.equal(heartbeatAge(rec, 70_000), 60_000);
    assert.equal(isStale(rec, 70_000, 420_000), false);
    assert.equal(isStale(rec, 10_000 + 421_000, 420_000), true);
    assert.equal(heartbeatAge(null, 1), Number.POSITIVE_INFINITY);
    assert.equal(isStale(null, 1, 420_000), true);
  });
});

describe("parseEvent / parseKill", () => {
  it("parses stamps and falls back to receive time", () => {
    assert.deepEqual(parseEvent("2026-11-04T02:00:00Z needle silent", 5), {
      t: Date.parse("2026-11-04T02:00:00Z"),
      text: "needle silent",
    });
    assert.deepEqual(parseEvent("  plain  ", 5), { t: 5, text: "plain" });
  });

  it("requires the confirm step and a reason", () => {
    assert.deepEqual(parseKill({ confirm: "kill", reason: " fat finger " }), { reason: "fat finger" });
    assert.throws(() => parseKill({ reason: "x" }), ValidationError);
    assert.throws(() => parseKill({ confirm: "kill", reason: "   " }), ValidationError);
    assert.throws(() => parseKill("kill"), ValidationError);
  });
});
