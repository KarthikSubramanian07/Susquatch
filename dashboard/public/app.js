// Susquatch dashboard client. No framework, no build step, no innerHTML: every value from the server is
// rendered with textContent. Polls /api/status every 20 s while visible, /api/summary every 5 min.

const REFRESH_MS = 20_000;
const SUMMARY_MS = 5 * 60_000;
const STAGES = ["forage", "stalk", "emerge", "locked"];
const START_EQUITY = 100_000;
const RISK = /freez|froze|breaker|kill|demot|reject|stale|cancel-all|halt|lease|restart|refused|quota/i;

const $ = (id) => document.getElementById(id);
const state = { status: null, kill: null, summary: null, fetchedAt: 0, serverNow: 0, stale: false, error: null };

// ── formatting ───────────────────────────────────────────────────────────
const fmt2 = new Intl.NumberFormat("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const fmt0 = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });
const MINUS = "−";
const money = (n) => fmt2.format(Math.abs(n));
const signed = (n) => (n > 0 ? "+" : n < 0 ? MINUS : "±") + money(n);
const tone = (n) => (n > 0 ? "up" : n < 0 ? "down" : "flat");
const nsToMs = (ns) => (ns === undefined || ns === null ? null : Number(BigInt(ns) / 1_000_000n));
const time = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
const dayTime = (ms) =>
  new Date(ms).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
const tz = new Intl.DateTimeFormat([], { timeZoneName: "short" }).formatToParts(new Date()).find((p) => p.type === "timeZoneName")?.value ?? "";

function ago(ms) {
  if (ms === null || !Number.isFinite(ms)) return "never";
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 90) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 90) return `${m}m ago`;
  const h = Math.floor(m / 60);
  return h < 48 ? `${h}h ${String(m % 60).padStart(2, "0")}m ago` : `${Math.floor(h / 24)}d ${h % 24}h ago`;
}

/** Tiny element builder: el("td", {class: "r num"}, "1.00", childNode, ...). Strings become text nodes. */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") node.className = v;
    else node.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

function setText(id, text, cls) {
  const node = $(id);
  node.textContent = text;
  if (cls !== undefined) node.className = cls;
}

// ── data ─────────────────────────────────────────────────────────────────
async function getJSON(path) {
  // Resolve against the origin: a page opened as https://user:pw@host/ would otherwise make fetch() throw.
  const res = await fetch(new URL(path, location.origin), { headers: { Accept: "application/json" }, cache: "no-store" });
  if (!res.ok) throw new Error(`${path}: ${res.status}`);
  return res.json();
}

async function refresh() {
  try {
    const body = await getJSON("/api/status");
    Object.assign(state, {
      status: body.status,
      kill: body.kill,
      stale: body.stale,
      serverNow: body.now,
      fetchedAt: Date.now(),
      error: null,
    });
  } catch (err) {
    state.error = err.message;
  }
  render();
}

async function refreshSummary() {
  try {
    state.summary = (await getJSON("/api/summary")).summary;
  } catch {
    /* keep the last one */
  }
  renderSummary();
}

/** Heartbeat age now, corrected for the time since we fetched (the server clock is the reference). */
function heartbeatAge() {
  if (!state.status) return null;
  return state.serverNow + (Date.now() - state.fetchedAt) - state.status.receivedAt;
}

// ── render ───────────────────────────────────────────────────────────────
function render() {
  const rec = state.status;
  const snap = rec?.snapshot ?? {};
  document.body.classList.toggle("is-stale", Boolean(state.stale));
  renderBar(snap);
  renderSilent();
  renderHero(rec, snap);
  renderHealth(snap.health ?? []);
  renderPositions(snap.positions ?? []);
  renderTrades(snap.recentTrades ?? []);
  renderEvents(rec?.events ?? []);
  renderSummary();
  renderFooter(snap);
}

function renderBar(snap) {
  const live = snap.mode === "live";
  setText("mode", live ? "live" : "paper", live ? "pill live" : "pill");
  const i = STAGES.indexOf(snap.stage);
  for (const li of $("stages").children) {
    const j = STAGES.indexOf(li.dataset.stage);
    li.className = i < 0 ? "" : j < i ? "past" : j === i ? "now" : "";
    li.toggleAttribute("aria-current", j === i);
  }
  setText("lease", snap.trader || "none");
  tickHeartbeat();

  const pending = state.kill?.killed && !snap.killed;
  const btn = $("kill-open");
  // The button carries the kill state itself; the chip only says "kill off" on wide screens.
  $("killstate").hidden = Boolean(snap.killed || pending);
  $("killed").hidden = !snap.killed;
  setText("killed-reason", snap.killReason ? `: ${snap.killReason}` : "");
  btn.className = `kill-btn${snap.killed ? " is-killed" : pending ? " is-pending" : ""}`;
  btn.disabled = Boolean(snap.killed || pending);
  btn.textContent = snap.killed ? "Killed" : pending ? "Kill sent" : "Kill";
  btn.title = snap.killed
    ? "Un-kill over SSH: make unkill"
    : pending
      ? `Requested ${state.kill.at ?? ""}; waiting for the server (~10 s poll + KV propagation)`
      : "Cancel all orders and freeze";
}

function tickHeartbeat() {
  const age = heartbeatAge();
  const dot = $("beat-dot");
  if (age === null) {
    dot.className = "dot dead";
    setText("beat", state.error ? "dashboard unreachable" : "no heartbeat yet");
    return;
  }
  const late = age > 1.5 * 150_000;
  dot.className = `dot ${state.stale ? "dead" : late ? "late" : "ok pulse"}`;
  setText("beat", ago(age));
}

function renderSilent() {
  const rec = state.status;
  $("silent").hidden = !state.stale;
  if (!state.stale) return;
  const title = $("silent-title");
  title.replaceChildren();
  if (!rec) {
    title.append("Server silent: no status has ever been received");
    return;
  }
  title.append("Server silent since ", el("span", { class: "num" }, dayTime(rec.receivedAt)), ` · ${ago(heartbeatAge()).replace(" ago", "")}`);
}

function splitMoney(n) {
  const [whole, cents] = fmt2.format(n).split(".");
  return [el("span", {}, whole), el("span", { class: "cents" }, `.${cents}`)];
}

function renderHero(rec, snap) {
  const eq = $("equity");
  eq.replaceChildren(...(snap.equity === undefined ? ["—"] : splitMoney(snap.equity)));
  setText("equity-sub", snap.cash === undefined ? "cash —" : `cash ${money(snap.cash)} · ${snap.mode ?? "paper"}`);
  const asof = $("asof");
  asof.hidden = !state.stale || !rec;
  if (rec) asof.textContent = `as of ${time(rec.receivedAt)}`;

  const day = snap.pnlDay;
  setText("pnl-day", day === undefined ? "—" : signed(day), `v ${day === undefined ? "" : tone(day)}`);
  setText("pnl-day-s", day === undefined || !snap.equity ? "" : `${signed((100 * day) / (snap.equity - day))}% today`);
  const total = snap.pnlTotal;
  setText("pnl-total", total === undefined ? "—" : signed(total), `v ${total === undefined ? "" : tone(total)}`);
  setText("pnl-total-s", total === undefined ? "" : `${signed((100 * total) / START_EQUITY)}% overall`);

  if (snap.rank) {
    const rank = $("rank");
    rank.textContent = fmt0.format(snap.rank);
    rank.className = "v";
    const pct = snap.fieldSize ? (100 * snap.rank) / snap.fieldSize : null;
    setText("rank-s", pct === null ? "" : `of ${fmt0.format(snap.fieldSize)} · top ${pct < 10 ? pct.toFixed(1) : Math.round(pct)}%`);
  } else {
    setText("rank", "unranked", "v flat");
    setText("rank-s", snap.fieldSize ? `field of ${fmt0.format(snap.fieldSize)}` : "");
  }
  renderSpark(rec?.history ?? []);
}

function renderSpark(history) {
  const svg = $("spark");
  svg.replaceChildren();
  const pts = history.filter((p) => Number.isFinite(p.e) && p.e > 0);
  const span = pts.length > 1 ? (pts.at(-1).t - pts[0].t) / 3_600_000 : 0;
  setText("spark-from", pts.length > 1 ? `equity · ${span >= 1 ? Math.round(span) + " h" : Math.round(span * 60) + " min"} ago` : "equity history appears after a few pushes");
  if (pts.length < 2) return;
  const W = 400;
  const H = 76;
  const t0 = pts[0].t;
  const t1 = pts.at(-1).t;
  let lo = Math.min(...pts.map((p) => p.e));
  let hi = Math.max(...pts.map((p) => p.e));
  const pad = Math.max((hi - lo) * 0.15, 1);
  lo -= pad;
  hi += pad;
  const x = (t) => (t1 === t0 ? W : ((t - t0) / (t1 - t0)) * W);
  const y = (e) => H - ((e - lo) / (hi - lo)) * H;
  const d = pts.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)},${y(p.e).toFixed(1)}`).join("");
  const ns = "http://www.w3.org/2000/svg";
  const mk = (tag, attrs) => {
    const n = document.createElementNS(ns, tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
    return n;
  };
  svg.append(
    mk("line", { x1: 0, x2: W, y1: y(pts[0].e), y2: y(pts[0].e) }),
    mk("path", { class: "area", d: `${d}L${W},${H}L0,${H}Z` }),
    mk("path", { class: "line", d }),
  );
}

function renderHealth(items) {
  const box = $("health");
  box.replaceChildren();
  const bad = items.filter((h) => h.state !== "ok").length;
  setText("health-sum", items.length ? (bad ? `${bad} need attention` : `all ${items.length} green`) : "");
  if (!items.length) {
    box.append(el("div", { class: "tile" }, el("div", { class: "detail" }, "No health reported yet.")));
    return;
  }
  const order = { down: 0, degraded: 1, ok: 2 };
  for (const h of [...items].sort((a, b) => (order[a.state] ?? 0) - (order[b.state] ?? 0))) {
    const last = nsToMs(h.lastOkNs);
    box.append(
      el(
        "div",
        { class: `tile ${h.state}` },
        el("div", { class: "name" }, el("span", { class: `dot ${h.state === "ok" ? "ok" : ""}` }), h.name),
        h.detail ? el("div", { class: "detail" }, h.detail) : el("div", { class: "detail" }, h.state),
        last ? el("div", { class: "when" }, `last ok ${ago(state.serverNow - last)}`) : null,
      ),
    );
  }
}

function sideOf(qty) {
  const q = Number(qty ?? 0);
  return q < 0 ? ["no", -q] : ["yes", q];
}

function renderPositions(rows) {
  const box = $("positions");
  box.replaceChildren();
  const unreal = rows.reduce((s, r) => s + (r.unrealized ?? 0), 0);
  setText("pos-sum", rows.length ? `${rows.length} open · unrealized ${signed(unreal)}` : "");
  if (!rows.length) {
    box.append(el("div", { class: "empty" }, "No positions. Sitting very still in the woods."));
    return;
  }
  const sorted = [...rows].sort((a, b) => Math.abs(b.unrealized ?? 0) - Math.abs(a.unrealized ?? 0));
  box.append(
    el(
      "div",
      { class: "table-wrap" },
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, "market"), el("th", { class: "r" }, "position"), el("th", { class: "r hide-sm" }, "avg"), el("th", { class: "r hide-sm" }, "mark"), el("th", { class: "r" }, "unrealized"))),
        el(
          "tbody",
          {},
          sorted.map((p) => {
            const [side, q] = sideOf(p.quantity);
            return el(
              "tr",
              {},
              el("td", { class: "title" }, p.title || p.exchangeId || "?"),
              el("td", { class: "r num" }, el("span", { class: `side ${side}` }, side.toUpperCase()), ` ${fmt0.format(q)}`),
              el("td", { class: "r num dim hide-sm" }, p.avgCost?.toFixed(3) ?? "—"),
              el("td", { class: "r num hide-sm" }, p.mark?.toFixed(3) ?? "—"),
              el("td", { class: `r num ${tone(p.unrealized ?? 0)}` }, p.unrealized === undefined ? "—" : signed(p.unrealized)),
            );
          }),
        ),
      ),
    ),
  );
}

function renderTrades(rows) {
  const box = $("trades");
  box.replaceChildren();
  if (!rows.length) {
    box.append(el("div", { class: "empty" }, "No trades yet."));
    return;
  }
  const sorted = [...rows].sort((a, b) => (nsToMs(b.tsNs) ?? 0) - (nsToMs(a.tsNs) ?? 0)).slice(0, 12);
  box.append(
    el(
      "div",
      { class: "table-wrap" },
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, "trade"), el("th", { class: "r" }, "qty @ px"))),
        el(
          "tbody",
          {},
          sorted.map((t) => {
            const side = (t.side ?? "").replace("SIDE_", "").toLowerCase();
            const action = (t.action ?? "").replace("ACTION_", "").toLowerCase();
            const ms = nsToMs(t.tsNs);
            return el(
              "tr",
              {},
              el("td", { class: "title" }, el("span", { class: "tag" }, `${ms ? time(ms) : "—"} · ${t.strategy ?? "?"} · ${action} ${side}`), el("br"), t.title ?? ""),
              el("td", { class: "r num" }, `${fmt0.format(Number(t.quantity ?? 0))} @ ${t.price?.toFixed(3) ?? "—"}`),
            );
          }),
        ),
      ),
    ),
  );
}

function renderEvents(events) {
  const box = $("events");
  box.replaceChildren();
  const now = state.serverNow || Date.now();
  const recent = events.filter((e) => e.t > now - 86_400_000).reverse();
  const risky = recent.filter((e) => RISK.test(e.text)).length;
  setText("events-sum", recent.length ? `${recent.length} · ${risky} risk` : "");
  if (!recent.length) {
    box.append(el("li", {}, el("time", {}, ""), el("span", {}, "Quiet day. Nothing notable in the last 24 hours.")));
    return;
  }
  for (const e of recent.slice(0, 40)) {
    const today = new Date(e.t).toDateString() === new Date(now).toDateString();
    const when = today ? time(e.t) : `${new Date(e.t).toLocaleDateString([], { weekday: "short" })} ${time(e.t)}`;
    box.append(el("li", { class: RISK.test(e.text) ? "risk" : "" }, el("time", { datetime: new Date(e.t).toISOString() }, when), el("span", {}, e.text)));
  }
}

function renderSummary() {
  const s = state.summary;
  const snap = state.status?.snapshot ?? {};
  const live = snap.actionNeeded ?? [];
  const items = [...live];
  for (const a of s?.actionNeeded ?? []) if (!items.includes(a) && !/^Server silent since/.test(a)) items.push(a);
  if (state.stale && state.status) items.unshift(`Server silent since ${dayTime(state.status.receivedAt)}. SSH in: make ps (restore runbook in docs/RUNBOOK.md).`);

  const action = $("action");
  action.replaceChildren();
  action.className = items.length ? "action" : "action clear";
  action.append(el("h3", {}, items.length ? "ACTION NEEDED" : "action needed"));
  action.append(items.length ? el("ul", {}, items.map((i) => el("li", {}, i))) : el("p", {}, "Nothing needs you today."));

  setText("summary-date", s ? new Date(`${s.date}T12:00:00Z`).toLocaleDateString([], { weekday: "short", month: "short", day: "numeric", timeZone: "UTC" }) : "no daily summary yet");
  const link = $("summary-link");
  link.replaceChildren();
  if (s?.posted?.url) link.append(el("a", { href: s.posted.url, target: "_blank", rel: "noopener noreferrer" }, "posted to GitHub ↗"));
  else if (s) link.append(`generated ${time(Date.parse(s.generatedAt))}`);

  const digest = $("digest");
  digest.replaceChildren();
  const row = (k, ...v) => el("div", { class: `digest-row ${["risk", "health"].includes(k) ? "" : "opt"}` }, el("span", { class: "k" }, k), el("span", { class: "v" }, ...v));
  if (!s) {
    digest.append(row("summary", "The daily Worker posts at 14:00 UTC. Live numbers are below."));
    return;
  }
  const h = s.headline;
  digest.append(
    row("24h", h.change24h === null ? "—" : el("span", { class: `num ${tone(h.change24h)}` }, signed(h.change24h)), h.equity === null ? "" : ` → ${money(h.equity)}`),
    row("risk", s.risk.length ? `${s.risk.length} event${s.risk.length > 1 ? "s" : ""}: ${s.risk.slice(-2).join(" · ")}` : "no freezes, breakers or kills"),
    row("notable", s.notable.length ? s.notable.slice(-2).join(" · ") : "nothing notable"),
    row("health", `${s.health.ok}/${s.health.total} ok`, s.health.issues.length ? ` · ${s.health.issues[0]}` : ""),
    row("backups", s.backup),
  );
  if (s.weeklyReview) digest.append(row("weekly", el("a", { href: s.weeklyReview.url, target: "_blank", rel: "noopener noreferrer" }, s.weeklyReview.title)));
}

function renderFooter(snap) {
  const h = snap.header ?? {};
  const parts = [
    `times in ${tz}`,
    state.error ? `last fetch failed: ${state.error}` : `refreshed ${ago(Date.now() - state.fetchedAt)}`,
    h.producer ? `push ${h.producer.name}@${h.producer.gitSha ?? "?"} #${h.seq ?? "?"}` : null,
    "no alerts, by design: one daily summary",
  ].filter(Boolean);
  $("footer").replaceChildren(...parts.map((p) => el("span", {}, p)));
}

// ── kill ─────────────────────────────────────────────────────────────────
function wireKill() {
  const dialog = $("kill-dialog");
  const reason = $("kill-reason");
  const confirm = $("kill-confirm");
  const err = $("kill-err");
  $("kill-open").addEventListener("click", () => {
    reason.value = "";
    confirm.disabled = true;
    err.hidden = true;
    dialog.showModal();
    reason.focus();
  });
  $("kill-cancel").addEventListener("click", () => dialog.close());
  reason.addEventListener("input", () => {
    confirm.disabled = reason.value.trim().length === 0;
  });
  $("kill-form").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    confirm.disabled = true;
    confirm.textContent = "Sending…";
    try {
      const res = await fetch(new URL("/api/kill", location.origin), {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-Susq-Intent": "kill" },
        body: JSON.stringify({ confirm: "kill", reason: reason.value.trim() }),
      });
      if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error ?? `HTTP ${res.status}`);
      state.kill = await res.json();
      dialog.close();
      render();
    } catch (e) {
      err.textContent = `Kill did not go through: ${e.message}. Use SSH: touch ops/KILL`;
      err.hidden = false;
      confirm.disabled = false;
    } finally {
      confirm.textContent = "Kill now";
    }
  });
}

// ── boot ─────────────────────────────────────────────────────────────────
wireKill();
refresh();
refreshSummary();
setInterval(() => document.visibilityState === "visible" && refresh(), REFRESH_MS);
setInterval(() => document.visibilityState === "visible" && refreshSummary(), SUMMARY_MS);
setInterval(tickHeartbeat, 1000);
document.addEventListener("visibilitychange", () => document.visibilityState === "visible" && refresh());
