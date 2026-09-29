/**
 * The dashboard's data model: what lives in KV, and how a pushed StatusSnapshot becomes a status record.
 *
 * `Snapshot` is the proto3-JSON form of `susq.v1.StatusSnapshot` (contracts/proto/susq/v1/ops.proto): camelCase
 * names, int64 as strings, default values omitted. It is validated structurally here rather than generated,
 * because the Workers bundle has no protobuf runtime. `test/model.test.ts` parses the golden fixture
 * `contracts/fixtures/status_snapshot.json` so schema drift breaks CI instead of the dashboard.
 *
 * KV keys (free tier: 1,000 writes/day, so every push is exactly ONE write):
 *   status             StatusRecord: latest snapshot + 24 h equity history + recent events
 *   kill               KillRecord: set by the dashboard, read by the server's kill poller
 *   summary:latest     Summary written by the daily Worker
 *   summary:YYYY-MM-DD one per day (kept 60 days)
 */

export interface Header {
  schemaVersion?: number;
  producedAtNs?: string | number;
  producer?: { name?: string; gitSha?: string };
  seq?: string | number;
}

export interface HealthItem {
  name: string;
  state: string; // ok | degraded | down
  detail?: string;
  lastOkNs?: string | number;
}

export interface Position {
  exchangeId?: string;
  title?: string;
  quantity?: string | number; // +YES / -NO
  avgCost?: number;
  mark?: number;
  unrealized?: number;
}

export interface Trade {
  tsNs?: string | number;
  strategy?: string;
  title?: string;
  side?: string; // SIDE_YES | SIDE_NO
  action?: string; // ACTION_BUY | ACTION_SELL
  quantity?: string | number;
  price?: number;
}

export interface Snapshot {
  header?: Header;
  mode?: string;
  stage?: string;
  trader?: string;
  equity?: number;
  cash?: number;
  pnlDay?: number;
  pnlTotal?: number;
  rank?: number;
  fieldSize?: number;
  killed?: boolean;
  killReason?: string;
  health?: HealthItem[];
  positions?: Position[];
  recentTrades?: Trade[];
  actionNeeded?: string[];
  events?: string[];
}

/** One equity sample per push, for the sparkline and the daily summary's 24 h change. */
export interface Point {
  t: number; // ms since epoch (server receive time)
  e: number; // equity
  d: number; // day P&L
  p: number; // total P&L
  r: number; // rank (0 = unranked)
}

export interface TimedEvent {
  t: number;
  text: string;
}

export interface StatusRecord {
  v: 1;
  receivedAt: number; // ms; the heartbeat the dead-man logic watches
  snapshot: Snapshot;
  history: Point[];
  events: TimedEvent[];
}

export interface KillRecord {
  killed: boolean;
  reason?: string;
  at?: string; // ISO time the human pressed it
  by?: string;
}

export interface Summary {
  date: string; // YYYY-MM-DD (UTC)
  generatedAt: string;
  silent: { since: string; minutes: number } | null;
  actionNeeded: string[];
  headline: {
    equity: number | null;
    change24h: number | null;
    pnlDay: number | null;
    pnlTotal: number | null;
    rank: number | null;
    fieldSize: number | null;
    stage: string;
    mode: string;
    trader: string;
    killed: boolean;
  };
  notable: string[];
  risk: string[];
  health: { ok: number; total: number; issues: string[] };
  backup: string;
  weeklyReview: { title: string; url: string } | null;
  markdown: string;
  posted?: { url: string } | null;
}

export const LIMITS = {
  bodyBytes: 256 * 1024,
  historyPoints: 900, // ~30 h at the 120-150 s push cadence
  events: 300,
  positions: 400,
  trades: 200,
  health: 64,
  actionNeeded: 50,
  pushEvents: 200,
  text: 500,
  minPushIntervalMs: 100_000, // protects the KV write budget from a runaway pusher
} as const;

export const KEYS = {
  status: "status",
  kill: "kill",
  summaryLatest: "summary:latest",
  summaryDay: (date: string) => `summary:${date}`,
} as const;

const MODES = new Set(["paper", "live"]);
const STATES = new Set(["ok", "degraded", "down"]);

export class ValidationError extends Error {}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function num(v: unknown, field: string): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isFinite(v)) throw new ValidationError(`${field}: not a finite number`);
  return v;
}

function int64(v: unknown, field: string): string | number | undefined {
  if (v === undefined) return undefined;
  if (typeof v === "number" && Number.isInteger(v)) return v;
  if (typeof v === "string" && /^-?\d{1,20}$/.test(v)) return v;
  throw new ValidationError(`${field}: not an int64`);
}

function str(v: unknown, field: string, max: number = LIMITS.text): string | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "string") throw new ValidationError(`${field}: not a string`);
  return v.length > max ? v.slice(0, max) : v;
}

function list<T>(v: unknown, field: string, max: number, each: (x: unknown, i: number) => T): T[] | undefined {
  if (v === undefined) return undefined;
  if (!Array.isArray(v)) throw new ValidationError(`${field}: not a list`);
  if (v.length > max) throw new ValidationError(`${field}: more than ${max} items`);
  return v.map(each);
}

/** Validate a pushed snapshot. Unknown fields are dropped (forward compatible); wrong types are rejected. */
export function parseSnapshot(raw: unknown): Snapshot {
  if (!isObj(raw)) throw new ValidationError("body: not an object");
  const h = raw.header;
  if (!isObj(h)) throw new ValidationError("header: missing");
  if (h.schemaVersion !== 1) throw new ValidationError(`header.schemaVersion: expected 1`);
  const mode = str(raw.mode, "mode") ?? "paper";
  if (!MODES.has(mode)) throw new ValidationError("mode: expected paper|live");
  const producer = isObj(h.producer) ? h.producer : {};
  return {
    header: {
      schemaVersion: 1,
      producedAtNs: int64(h.producedAtNs, "header.producedAtNs"),
      producer: { name: str(producer.name, "producer.name", 64), gitSha: str(producer.gitSha, "producer.gitSha", 64) },
      seq: int64(h.seq, "header.seq"),
    },
    mode,
    stage: str(raw.stage, "stage", 32),
    trader: str(raw.trader, "trader", 64),
    equity: num(raw.equity, "equity"),
    cash: num(raw.cash, "cash"),
    pnlDay: num(raw.pnlDay, "pnlDay"),
    pnlTotal: num(raw.pnlTotal, "pnlTotal"),
    rank: num(raw.rank, "rank"),
    fieldSize: num(raw.fieldSize, "fieldSize"),
    killed: raw.killed === undefined ? undefined : raw.killed === true,
    killReason: str(raw.killReason, "killReason"),
    health: list(raw.health, "health", LIMITS.health, (x, i) => {
      if (!isObj(x)) throw new ValidationError(`health[${i}]: not an object`);
      const state = str(x.state, `health[${i}].state`) ?? "down";
      return {
        name: str(x.name, `health[${i}].name`, 64) ?? "?",
        state: STATES.has(state) ? state : "down",
        detail: str(x.detail, `health[${i}].detail`),
        lastOkNs: int64(x.lastOkNs, `health[${i}].lastOkNs`),
      };
    }),
    positions: list(raw.positions, "positions", LIMITS.positions, (x, i) => {
      if (!isObj(x)) throw new ValidationError(`positions[${i}]: not an object`);
      return {
        exchangeId: str(x.exchangeId, `positions[${i}].exchangeId`, 64),
        title: str(x.title, `positions[${i}].title`, 200),
        quantity: int64(x.quantity, `positions[${i}].quantity`),
        avgCost: num(x.avgCost, `positions[${i}].avgCost`),
        mark: num(x.mark, `positions[${i}].mark`),
        unrealized: num(x.unrealized, `positions[${i}].unrealized`),
      };
    }),
    recentTrades: list(raw.recentTrades, "recentTrades", LIMITS.trades, (x, i) => {
      if (!isObj(x)) throw new ValidationError(`recentTrades[${i}]: not an object`);
      return {
        tsNs: int64(x.tsNs, `recentTrades[${i}].tsNs`),
        strategy: str(x.strategy, `recentTrades[${i}].strategy`, 64),
        title: str(x.title, `recentTrades[${i}].title`, 200),
        side: str(x.side, `recentTrades[${i}].side`, 32),
        action: str(x.action, `recentTrades[${i}].action`, 32),
        quantity: int64(x.quantity, `recentTrades[${i}].quantity`),
        price: num(x.price, `recentTrades[${i}].price`),
      };
    }),
    actionNeeded: list(raw.actionNeeded, "actionNeeded", LIMITS.actionNeeded, (x, i) => str(x, `actionNeeded[${i}]`) ?? ""),
    events: list(raw.events, "events", LIMITS.pushEvents, (x, i) => str(x, `events[${i}]`) ?? ""),
  };
}

const STAMP = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)\s+(.*)$/s;

/** "2026-10-05T03:12:00Z text" -> {t, text}; unstamped events get the receive time. */
export function parseEvent(line: string, fallback: number): TimedEvent {
  const m = STAMP.exec(line);
  if (m) {
    const t = Date.parse(m[1]!);
    if (Number.isFinite(t)) return { t, text: m[2]!.trim() };
  }
  return { t: fallback, text: line.trim() };
}

/** Fold one validated snapshot into the stored record (pure; the caller does the single KV write). */
export function applyPush(prev: StatusRecord | null, snap: Snapshot, now: number): StatusRecord {
  const history = prev?.history ?? [];
  const point: Point = {
    t: now,
    e: snap.equity ?? 0,
    d: snap.pnlDay ?? 0,
    p: snap.pnlTotal ?? 0,
    r: snap.rank ?? 0,
  };
  const incoming = (snap.events ?? []).filter((e) => e.trim()).map((e) => parseEvent(e, now));
  const events = [...(prev?.events ?? []), ...incoming].slice(-LIMITS.events);
  const { events: _pushed, ...rest } = snap;
  return {
    v: 1,
    receivedAt: now,
    snapshot: rest,
    history: [...history, point].slice(-LIMITS.historyPoints),
    events,
  };
}

/** Milliseconds since the server last pushed; Infinity when it never has. */
export function heartbeatAge(rec: StatusRecord | null, now: number): number {
  return rec ? Math.max(0, now - rec.receivedAt) : Number.POSITIVE_INFINITY;
}

export function isStale(rec: StatusRecord | null, now: number, staleAfterMs: number): boolean {
  return heartbeatAge(rec, now) > staleAfterMs;
}

export function parseKill(raw: unknown): { reason: string } {
  if (!isObj(raw)) throw new ValidationError("body: not an object");
  if (raw.confirm !== "kill") throw new ValidationError("confirm: the second step must send confirm='kill'");
  const reason = (str(raw.reason, "reason", 300) ?? "").trim();
  if (!reason) throw new ValidationError("reason: required");
  return { reason };
}
