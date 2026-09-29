#!/usr/bin/env node
// Seed LOCAL KV (never remote) for `npm run dev` with a realistic day, built through the real code paths:
// applyPush() for the status record and the Worker's compose() for the summary.
//
//   node scripts/seed.mjs normal   # a good day in the Stalk stage
//   node scripts/seed.mjs killed   # dashboard Kill pressed and applied
//   node scripts/seed.mjs pending  # Kill pressed, server has not applied it yet
//   node scripts/seed.mjs stale    # server silent for 10h48m (dead-man state)
//   node scripts/seed.mjs empty    # nothing ever pushed
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { KEYS, applyPush, parseSnapshot } from "../src/model.ts";
import { compose } from "../../worker/src/summary.ts";

const variant = process.argv[2] ?? "normal";
const here = import.meta.dirname;
const base = JSON.parse(readFileSync(join(here, "../../contracts/fixtures/status_snapshot.json"), "utf8"));
const MIN = 60_000;
const now = Date.now();
const receivedAt = variant === "stale" ? now - (10 * 60 + 48) * MIN : now - 38_000;
const ns = (ms) => String(BigInt(Math.round(ms)) * 1_000_000n);

// A deterministic 26 h random walk ending at today's equity, one point per push.
function history(end) {
  let seed = 7;
  const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647) - 0.5;
  const pts = [];
  let e = end - 2861.4;
  for (let t = receivedAt - 26 * 60 * MIN; t <= receivedAt; t += 150_000) {
    e += rand() * 140 + 2861.4 / 624;
    pts.push({ t, e: Math.round(e * 100) / 100, d: 0, p: e - 100_000, r: 0 });
  }
  // Tilt the walk so it lands exactly on today's equity (no jump at the end).
  const miss = end - pts[pts.length - 1].e;
  return pts.map((p, i) => ({ ...p, e: Math.round((p.e + (miss * i) / (pts.length - 1)) * 100) / 100 }));
}

const positions = [
  { exchangeId: "381", title: "Democrats win the NH Senate race", quantity: "-37", avgCost: 0.145, mark: 0.14, unrealized: 0.19 },
  { exchangeId: "412", title: "Republicans win the House (≥ 218 seats)", quantity: "2400", avgCost: 0.412, mark: 0.447, unrealized: 84.0 },
  { exchangeId: "517", title: "Maine Senate: Mills (D)", quantity: "1310", avgCost: 0.305, mark: 0.331, unrealized: 34.06 },
  { exchangeId: "533", title: "Nebraska Senate: Osborn (I)", quantity: "860", avgCost: 0.21, mark: 0.195, unrealized: -12.9 },
  { exchangeId: "602", title: "Michigan Senate: Democrats", quantity: "-540", avgCost: 0.36, mark: 0.352, unrealized: 4.32 },
  { exchangeId: "640", title: "Ohio Senate: Brown (D)", quantity: "1175", avgCost: 0.265, mark: 0.29, unrealized: 29.38 },
];
const trades = [
  ["racer", "Maine Senate: Mills (D)", "SIDE_YES", "ACTION_BUY", "310", 0.33, 11],
  ["sniper", "Ohio Senate: Brown (D)", "SIDE_YES", "ACTION_BUY", "175", 0.27, 47],
  ["racer", "Democrats win the NH Senate race", "SIDE_NO", "ACTION_BUY", "37", 0.855, 96],
  ["mm", "Republicans win the House (≥ 218 seats)", "SIDE_YES", "ACTION_SELL", "120", 0.45, 142],
  ["scenario", "Nebraska Senate: Osborn (I)", "SIDE_YES", "ACTION_BUY", "260", 0.205, 231],
].map(([strategy, title, side, action, quantity, price, minsAgo]) => ({
  tsNs: ns(receivedAt - minsAgo * MIN), strategy, title, side, action, quantity, price,
}));
const health = [
  ["platform", "ok", ""], ["realtime", "ok", "5 channels joined"], ["kalshi", "ok", "fair values 4s old"],
  ["targets", "ok", "targets-20261005T1302Z-9f1c2e4a"], ["needle", "ok", "idle until Nov 3"],
  ["status-writer", "ok", ""], ["backup", "ok", "last good backup 6h ago"], ["selfcheck", "ok", "7/7 ok"],
  ["kill-poller", "ok", ""],
].map(([name, state, detail]) => ({ name, state, detail, lastOkNs: ns(receivedAt - 4000) }));

const snap = {
  ...base,
  header: { ...base.header, producer: { name: "status-pusher", gitSha: "a1c9e07d52be" }, seq: "4127" },
  stage: "stalk",
  trader: "rust-live",
  equity: 131_486.25,
  cash: 94_310.8,
  pnlDay: 1_842.6,
  pnlTotal: 31_486.25,
  rank: 37,
  fieldSize: 2_214,
  positions,
  recentTrades: trades,
  health,
  actionNeeded: [],
  events: [],
};
let killRecord = null;
if (variant === "killed") {
  snap.killed = true;
  snap.killReason = "needle posteriors look inverted on ME-02";
  killRecord = { killed: true, reason: snap.killReason, at: new Date(receivedAt - 3 * MIN).toISOString(), by: "dashboard" };
  snap.health = health.map((h) => (h.name === "platform" ? { ...h, state: "degraded", detail: "frozen: ops/KILL present" } : h));
}
if (variant === "pending") {
  killRecord = { killed: true, reason: "pressed from the phone", at: new Date(now - 20_000).toISOString(), by: "dashboard" };
}
if (variant === "stale") {
  snap.health = health.map((h) => (h.name === "kalshi" ? { ...h, state: "degraded", detail: "fair values 11m old: sniper paused" } : h));
  snap.actionNeeded = ["Rotate soon: github_token in 9d."];
}

const iso = (msAgo) => new Date(receivedAt - msAgo).toISOString().replace(/\.\d+Z$/, "Z");
const events = [
  [22 * 60, "nightly backup uploaded: susq-20261005T073000Z.tar.zst (41 MB)"],
  [9 * 60, "targets-20261005T1302Z accepted: 14 legs, P(1st) 0.071"],
  [5 * 60, "loss-per-hour breaker armed at 3.0% (was 2.5%)"],
  [4 * 60, "racer: 3 bundles on NH/ME relationship violations, +212.40"],
  [96, "sniper paused 4 min: kalshi fair values stale (62s)"],
  [58, "first scenario tranche filled: 6/14 legs"],
  [11, "leaderboard: rank 41 -> 37"],
];
if (variant === "killed") events.push([3, "KILL applied from dashboard: needle posteriors look inverted on ME-02"], [2, "cancel-all: 23 open orders cancelled, frozen"]);
snap.events = events.map(([m, text]) => `${iso(m * MIN)} ${text}`);

let record = null;
if (variant !== "empty") {
  record = applyPush(null, parseSnapshot(snap), receivedAt);
  record.history = history(snap.equity);
}
const summary = compose(record, killRecord, {
  now: new Date(Math.min(now, Date.parse(new Date(now).toISOString().slice(0, 10) + "T14:00:00Z")) || now),
  deadmanMinutes: 20,
  dashboardUrl: "https://susquatch.pages.dev",
  weeklyReview: { title: "2026-W40", url: "https://github.com/KarthikSubramanian07/susquatch-reports/blob/main/weekly/2026-W40.md" },
});
summary.posted = { url: "https://github.com/KarthikSubramanian07/susquatch-reports/issues/1#issuecomment-1" };

const dir = mkdtempSync(join(tmpdir(), "susq-seed-"));
function put(key, value) {
  const file = join(dir, key.replace(/[^a-z0-9]/gi, "_"));
  writeFileSync(file, JSON.stringify(value));
  execFileSync("npx", ["wrangler", "kv", "key", "put", key, "--path", file, "--binding", "SUSQ_KV", "--local", "--persist-to", ".wrangler/state"], {
    cwd: join(here, ".."),
    stdio: ["ignore", "ignore", "inherit"],
  });
}
function del(key) {
  execFileSync("npx", ["wrangler", "kv", "key", "delete", key, "--binding", "SUSQ_KV", "--local", "--persist-to", ".wrangler/state"], {
    cwd: join(here, ".."),
    stdio: "ignore",
  });
}

if (record) put(KEYS.status, record);
else del(KEYS.status);
if (killRecord) put(KEYS.kill, killRecord);
else del(KEYS.kill);
put(KEYS.summaryLatest, summary);
console.log(`seeded local KV: ${variant}`);
