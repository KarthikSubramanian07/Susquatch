/**
 * Compose Susquatch's daily summary from what's in KV. Pure: no I/O, `now` is passed in.
 *
 * Order matters (OPS_FREE_AUTONOMY §B): the dead-man line and ACTION NEEDED come first, and they are the
 * only parts that ask anything of a human. Then money, stage, risk events, notable events, health, backups
 * and the weekly review link.
 */
import type { KillRecord, StatusRecord, Summary, TimedEvent } from "../../dashboard/src/model.ts";

export interface ComposeOptions {
  now: Date;
  deadmanMinutes: number;
  dashboardUrl: string;
  weeklyReview?: { title: string; url: string } | null;
}

const DAY_MS = 86_400_000;
const RISK = /freez|froze|breaker|kill|demot|reject|stale|cancel-all|halt|lease|restart|refused|quota/i;

const money = (n: number) =>
  n.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const signed = (n: number) => `${n > 0 ? "+" : n < 0 ? "−" : "±"}${money(Math.abs(n))}`;
const hhmm = (t: number) => new Date(t).toISOString().slice(11, 16) + "Z";
const stamp = (t: number) => new Date(t).toISOString().replace("T", " ").slice(0, 16) + " UTC";

export function duration(minutes: number): string {
  if (minutes < 60) return `${Math.round(minutes)}m`;
  const h = Math.floor(minutes / 60);
  const m = Math.round(minutes % 60);
  return h >= 48 ? `${Math.floor(h / 24)}d ${h % 24}h` : `${h}h ${String(m).padStart(2, "0")}m`;
}

/** Equity change over the last 24 h from the push history (null if we don't have a sample that old). */
export function change24h(rec: StatusRecord, now: number): number | null {
  const cutoff = now - DAY_MS;
  let base: number | null = null;
  for (const p of rec.history) {
    if (p.t <= cutoff) base = p.e;
    else break;
  }
  const latest = rec.snapshot.equity;
  return base === null || latest === undefined ? null : latest - base;
}

function lastDay(events: TimedEvent[], now: number): TimedEvent[] {
  return events.filter((e) => e.t > now - DAY_MS && e.t <= now + 60_000);
}

export function compose(rec: StatusRecord | null, kill: KillRecord | null, opts: ComposeOptions): Summary {
  const now = opts.now.getTime();
  const date = opts.now.toISOString().slice(0, 10);
  const snap = rec?.snapshot ?? {};

  // ── dead-man ──
  let silent: Summary["silent"] = null;
  if (!rec) silent = { since: "never", minutes: -1 };
  else {
    const minutes = (now - rec.receivedAt) / 60_000;
    if (minutes > opts.deadmanMinutes) silent = { since: new Date(rec.receivedAt).toISOString(), minutes };
  }

  // ── ACTION NEEDED ──
  const action: string[] = [];
  if (silent && silent.since === "never") {
    action.push("The server has never pushed a status. Set SUSQ_STATUS_PUSH_TOKEN on both sides (docs/OPS.md, Secrets).");
  } else if (silent) {
    action.push(
      `Server silent since ${stamp(Date.parse(silent.since))} (${duration(silent.minutes)}). ` +
        "SSH in and run `make ps`; if the host is gone, follow docs/RUNBOOK.md (Restore).",
    );
  }
  if (kill?.killed && !snap.killed && !silent) {
    const at = kill.at ? ` at ${stamp(Date.parse(kill.at))}` : "";
    action.push(`Kill was pressed${at} but the server hasn't confirmed it. Check the kill poller (make logs).`);
  }
  for (const item of snap.actionNeeded ?? []) if (item && !action.includes(item)) action.push(item);

  // ── the rest ──
  const events = rec ? lastDay(rec.events, now) : [];
  const risk = events.filter((e) => RISK.test(e.text)).map((e) => `${hhmm(e.t)} ${e.text}`);
  const notable = events.filter((e) => !RISK.test(e.text)).slice(-12).map((e) => `${hhmm(e.t)} ${e.text}`);
  const health = snap.health ?? [];
  const issues = health.filter((h) => h.state !== "ok").map((h) => `${h.name}: ${h.state}${h.detail ? ` (${h.detail})` : ""}`);
  const backupItem = health.find((h) => h.name === "backup");
  const backup = backupItem ? `${backupItem.state}${backupItem.detail ? `, ${backupItem.detail}` : ""}` : "not reported";

  const summary: Summary = {
    date,
    generatedAt: opts.now.toISOString(),
    silent,
    actionNeeded: action,
    headline: {
      equity: snap.equity ?? null,
      change24h: rec ? change24h(rec, now) : null,
      pnlDay: snap.pnlDay ?? null,
      pnlTotal: snap.pnlTotal ?? null,
      rank: snap.rank ? snap.rank : null,
      fieldSize: snap.fieldSize ?? null,
      stage: snap.stage ?? "unknown",
      mode: snap.mode ?? "unknown",
      trader: snap.trader ?? "none",
      killed: Boolean(snap.killed),
    },
    notable,
    risk,
    health: { ok: health.length - issues.length, total: health.length, issues },
    backup,
    weeklyReview: opts.weeklyReview ?? null,
    markdown: "",
  };
  summary.markdown = toMarkdown(summary, opts.dashboardUrl, snap.killReason);
  return summary;
}

export function toMarkdown(s: Summary, dashboardUrl: string, killReason?: string): string {
  const day = new Date(`${s.date}T12:00:00Z`).toLocaleDateString("en-US", {
    weekday: "short",
    month: "short",
    day: "numeric",
    year: "numeric",
    timeZone: "UTC",
  });
  const h = s.headline;
  const out: string[] = [`### Susquatch · ${day}`, ""];

  if (s.silent) {
    out.push(
      s.silent.since === "never"
        ? "> **Server silent: no status has ever been received.**"
        : `> **Server silent since ${stamp(Date.parse(s.silent.since))}** (${duration(s.silent.minutes)}). ` +
            "Everything below is the last known state.",
      "",
    );
  }

  out.push("**ACTION NEEDED**");
  out.push(...(s.actionNeeded.length ? s.actionNeeded.map((a) => `- [ ] ${a}`) : ["- nothing today"]), "");

  const rank = h.rank ? `${h.rank.toLocaleString("en-US")} / ${(h.fieldSize ?? 0).toLocaleString("en-US")}` : "unranked";
  out.push(
    "| equity | 24h | day P&L | total P&L | rank |",
    "|---:|---:|---:|---:|---:|",
    `| ${h.equity === null ? "n/a" : money(h.equity)} | ${h.change24h === null ? "n/a" : signed(h.change24h)} | ` +
      `${h.pnlDay === null ? "n/a" : signed(h.pnlDay)} | ${h.pnlTotal === null ? "n/a" : signed(h.pnlTotal)} | ${rank} |`,
    "",
    `**stage** ${h.stage} · **mode** ${h.mode} · **trader** ${h.trader} · **kill** ` +
      (h.killed ? `ON${killReason ? ` (${killReason})` : ""}` : "off"),
    "",
  );

  out.push("**Freezes & risk events (24h)**");
  out.push(...(s.risk.length ? s.risk.map((r) => `- ${r}`) : ["- none"]), "");
  if (s.notable.length) out.push("**Notable (24h)**", ...s.notable.map((n) => `- ${n}`), "");

  out.push(
    `**Health** ${s.health.ok}/${s.health.total} ok` + (s.health.issues.length ? `: ${s.health.issues.join("; ")}` : ""),
    `**Backups** ${s.backup}`,
  );
  if (s.weeklyReview) out.push(`**Weekly review** [${s.weeklyReview.title}](${s.weeklyReview.url})`);
  out.push("", `[dashboard](${dashboardUrl}) · generated ${s.generatedAt.slice(0, 16).replace("T", " ")} UTC`);
  return out.join("\n");
}
