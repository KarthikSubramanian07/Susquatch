// Susquatch dashboard client. No framework, no build step, no innerHTML: every value from the server is
// rendered with textContent. Polls /api/status every 20 s while visible, /api/summary every 5 min.
//
// Motion policy: numbers tween when they change, lists rise once on first paint, the sparkline draws in once,
// and the heartbeat dot rings when a fresh push lands. Everything collapses to instant under reduced motion.

const REFRESH_MS = 20_000;
const SUMMARY_MS = 5 * 60_000;
const STAGES = ["forage", "stalk", "emerge", "locked"];
const START_EQUITY = 100_000;
const RISK = /freez|froze|breaker|kill|demot|reject|stale|cancel-all|halt|lease|restart|refused|quota/i;
const HOLD_MS = 900;
// The Cup's calendar (docs/API.md, crates/live/src/status.rs). Fixed; nothing here is a guess.
const CAL = {
  open: Date.parse("2026-10-01T16:00:00Z"),
  stalk: Date.parse("2026-10-21T00:00:00Z"),
  emerge: Date.parse("2026-11-04T00:00:00Z"),
  lastOrder: Date.parse("2026-11-04T16:55:00Z"),
  lock: Date.parse("2026-11-04T17:00:00Z"),
};

const $ = (id) => document.getElementById(id);
const reduced = matchMedia("(prefers-reduced-motion: reduce)");
const state = { status: null, kill: null, summary: null, fetchedAt: 0, serverNow: 0, stale: false, error: null, firstPaint: true, lastSeq: null };

// ── formatting ───────────────────────────────────────────────────────────
const fmt2 = new Intl.NumberFormat("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const fmt0 = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });
const MINUS = "−";
const money = (n) => fmt2.format(Math.abs(n));
const signed = (n) => (n > 0 ? "+" : n < 0 ? MINUS : "±") + money(n);
const tone = (n) => (n > 0 ? "up" : n < 0 ? "down" : "flat");
const nsToMs = (ns) => (ns === undefined || ns === null ? null : Number(BigInt(ns) / 1_000_000n));
const time = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
const dayTime = (ms) => new Date(ms).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
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

/** "36d 04h 12m" style countdown, coarser the further away it is. */
function until(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const d = Math.floor(s / 86_400);
  const h = Math.floor((s % 86_400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d}d ${String(h).padStart(2, "0")}h ${String(m).padStart(2, "0")}m`;
  if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m ${String(s % 60).padStart(2, "0")}s`;
  return `${m}m ${String(s % 60).padStart(2, "0")}s`;
}

/** Tiny element builder: el("td", {class: "r num"}, "1.00", childNode, ...). Strings become text nodes. */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") node.className = v;
    else if (k === "style") node.style.cssText = v;
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

/** Mark children with --i so CSS can stagger them, only on first paint (re-renders must not re-animate). */
function stagger(nodes, cap = 14) {
  if (!state.firstPaint) return nodes;
  nodes.forEach((n, i) => {
    n.classList.add("enter");
    n.style.setProperty("--i", Math.min(i, cap));
  });
  return nodes;
}

// ── number tweens ────────────────────────────────────────────────────────
const easeOutExpo = (t) => (t >= 1 ? 1 : 1 - Math.pow(2, -10 * t));
const tweens = new WeakMap();

/** Animate a number in `node` from its last value to `to`, rendering with `paint(value)`. */
function tween(node, to, paint, { from = null, ms = 700 } = {}) {
  const prev = tweens.get(node);
  const start = from ?? prev?.value ?? to;
  node.classList.remove("skel");
  if (reduced.matches || start === to || !Number.isFinite(start)) {
    tweens.set(node, { value: to });
    paint(to);
    return;
  }
  if (prev?.raf) cancelAnimationFrame(prev.raf);
  const t0 = performance.now();
  const rec = { value: start, raf: 0 };
  tweens.set(node, rec);
  const step = (now) => {
    const k = easeOutExpo(Math.min(1, (now - t0) / ms));
    rec.value = start + (to - start) * k;
    paint(k === 1 ? to : rec.value);
    if (k < 1) rec.raf = requestAnimationFrame(step);
  };
  rec.raf = requestAnimationFrame(step);
}

function splitMoney(n) {
  const [whole, cents] = fmt2.format(n).split(".");
  return [el("span", {}, whole), el("span", { class: "cents" }, `.${cents}`)];
}

// ── data ─────────────────────────────────────────────────────────────────
async function getJSON(path) {
  // Resolve against the origin: a page opened as https://user:pw@host/ would otherwise make fetch() throw.
  const res = await fetch(new URL(path, location.origin), { headers: { Accept: "application/json" }, cache: "no-store" });
  if (res.status === 401) {
    // The session expired (or the password changed): go and sign in again, then come back here.
    location.replace(`/login?next=${encodeURIComponent(location.pathname + location.search)}`);
    throw new Error("signed out");
  }
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
  restartPollBar();
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

/** Best estimate of the server's "now", so the calendar never trusts the phone clock alone. */
function nowMs() {
  return state.serverNow ? state.serverNow + (Date.now() - state.fetchedAt) : Date.now();
}

function restartPollBar() {
  const bar = $("poll");
  bar.style.setProperty("--poll-ms", `${REFRESH_MS}ms`);
  bar.classList.remove("run");
  void bar.offsetWidth; // restart the CSS animation
  bar.classList.add("run");
}

// ── render ───────────────────────────────────────────────────────────────
function render() {
  const rec = state.status;
  const snap = rec?.snapshot ?? {};
  document.body.dataset.state = state.error && !rec ? "error" : state.stale ? "stale" : rec ? "live" : "empty";
  renderBar(snap);
  renderCalendar(snap);
  renderSilent();
  renderHero(rec, snap);
  renderHealth(snap.health ?? []);
  renderPositions(snap.positions ?? []);
  renderTrades(snap.recentTrades ?? []);
  renderEvents(rec?.events ?? []);
  renderSummary();
  renderFooter(snap);
  renderTitle(snap);
  state.firstPaint = false;
  document.body.dataset.ready = "1";
}

function renderBar(snap) {
  const live = snap.mode === "live";
  setText("mode", live ? "live" : "paper", live ? "pill live" : "pill");
  setText("lease", snap.trader || "none");

  // A new seq means a fresh push landed while the page was open: ring the dot once.
  const seq = snap.header?.seq ?? null;
  const fresh = state.lastSeq !== null && seq !== null && seq !== state.lastSeq;
  state.lastSeq = seq;
  tickHeartbeat(fresh);

  const pending = state.kill?.killed && !snap.killed;
  const btn = $("kill-open");
  $("killstate").hidden = Boolean(snap.killed || pending);
  $("killed").hidden = !snap.killed;
  setText("killed-reason", snap.killReason ? `: ${snap.killReason}` : "");
  btn.className = `kill-btn${snap.killed ? " is-killed" : pending ? " is-pending" : ""}`;
  btn.disabled = Boolean(snap.killed || pending);
  btn.textContent = snap.killed ? "Killed" : pending ? "Kill sent" : "Kill";
  btn.setAttribute("aria-expanded", "false");
  btn.title = snap.killed
    ? "Un-kill over SSH: make unkill"
    : pending
      ? `Requested ${state.kill.at ?? ""}; waiting for the server (~10 s poll + KV propagation)`
      : "Cancel all orders and freeze";

  const live = $("live");
  if (live) {
    const note = snap.killed
      ? `Susquatch killed${snap.killReason ? `: ${snap.killReason}` : ""}`
      : state.stale
        ? "Server silent. Numbers below are the last known state."
        : "";
    if (note && live.dataset.last !== note) {
      live.textContent = note;
      live.dataset.last = note;
    }
  }
}

function tickHeartbeat(fresh = false) {
  const age = heartbeatAge();
  const dot = $("beat-dot");
  if (age === null) {
    dot.className = "dot dead";
    setText("beat", state.error ? "dashboard unreachable" : "no heartbeat yet");
    return;
  }
  const late = age > 1.5 * 150_000;
  const cls = `dot ${state.stale ? "dead" : late ? "late" : "ok pulse"}`;
  if (fresh && !state.stale && !late) {
    dot.className = `${cls} fresh`;
    dot.addEventListener("animationend", () => dot.classList.remove("fresh"), { once: true });
  } else if (dot.className !== cls && !dot.classList.contains("fresh")) dot.className = cls;
  setText("beat", ago(age));
}

/** The calendar strip: proportional segments, a marker at "now", and a countdown to the next boundary. */
function renderCalendar(snap) {
  const now = nowMs();
  const bounds = [CAL.open, CAL.stalk, CAL.emerge, CAL.lastOrder];
  const stageNow = snap.stage && STAGES.includes(snap.stage) ? snap.stage : now < CAL.open ? null : STAGES[Math.min(3, bounds.filter((b) => now >= b).length - 1)];
  const idx = STAGES.indexOf(stageNow);
  const items = [...$("stages").children];
  const total = CAL.lastOrder - CAL.open;
  items.forEach((li, j) => {
    if (j < 3) {
      const a = bounds[j];
      const b = bounds[j + 1];
      li.style.setProperty("--w", ((b - a) / total) * 10 + 1);
      li.style.setProperty("--fill", now <= a ? 0 : now >= b ? 1 : (now - a) / (b - a));
    }
    li.className = idx < 0 ? "" : j < idx ? "past" : j === idx ? "now" : "";
    li.toggleAttribute("aria-current", j === idx);
  });
  positionMarker(now);

  const clock = $("phase-clock");
  clock.replaceChildren();
  if (now < CAL.open) clock.append("opens in ", el("b", {}, until(CAL.open - now)));
  else if (now < CAL.lastOrder) clock.append("locks in ", el("b", {}, until(CAL.lastOrder - now)), " · lock Nov 4 12:00 ET");
  else if (now < CAL.lock) clock.append(el("b", {}, "hard gate"), " · no new orders");
  else clock.append(el("b", {}, "locked"), " · rank is the balance after every market settles");
}

function positionMarker(now = nowMs()) {
  const items = [...$("stages").children];
  const bounds = [CAL.open, CAL.stalk, CAL.emerge, CAL.lastOrder];
  const inner = $("stages").parentElement.getBoundingClientRect();
  const rects = items.map((li) => li.getBoundingClientRect());
  let x;
  if (now <= CAL.open) x = rects[0].left;
  else if (now >= CAL.lastOrder) x = rects[2].right;
  else {
    const j = bounds.findIndex((b, i) => now >= b && now < bounds[i + 1]);
    const r = rects[j];
    x = r.left + ((now - bounds[j]) / (bounds[j + 1] - bounds[j])) * r.width;
  }
  $("phase-now").style.setProperty("--x", `${x - inner.left}px`);
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

function renderHero(rec, snap) {
  const eq = $("equity");
  if (snap.equity === undefined) {
    if (rec) {
      eq.classList.remove("skel");
      eq.textContent = "—";
    }
  } else {
    tween(eq, snap.equity, (v) => eq.replaceChildren(...splitMoney(v)), { from: state.firstPaint ? START_EQUITY : null, ms: 900 });
  }
  setText("equity-sub", snap.cash === undefined ? "cash —" : `cash ${money(snap.cash)} · ${snap.mode ?? "paper"}`);
  const asof = $("asof");
  asof.hidden = !state.stale || !rec;
  if (rec) asof.textContent = `as of ${time(rec.receivedAt)}`;

  const stat = (id, value, subtext) => {
    const node = $(id);
    if (value === undefined) {
      if (rec) {
        node.classList.remove("skel");
        node.textContent = "—";
        node.className = "v num";
      }
    } else {
      tween(node, value, (v) => {
        node.textContent = signed(v);
        node.className = `v num ${tone(value)}`;
      }, { from: state.firstPaint ? 0 : null });
    }
    setText(`${id}-s`, subtext);
  };
  const day = snap.pnlDay;
  stat("pnl-day", day, day === undefined || !snap.equity ? "" : `${signed((100 * day) / (snap.equity - day))}% today`);
  const total = snap.pnlTotal;
  stat("pnl-total", total, total === undefined ? "" : `${signed((100 * total) / START_EQUITY)}% overall`);

  const rank = $("rank");
  if (snap.rank) {
    tween(rank, snap.rank, (v) => {
      rank.textContent = fmt0.format(v);
      rank.className = "v num";
    }, { from: state.firstPaint ? snap.fieldSize || snap.rank : null });
    const pct = snap.fieldSize ? (100 * snap.rank) / snap.fieldSize : null;
    setText("rank-s", pct === null ? "" : `of ${fmt0.format(snap.fieldSize)} · top ${pct < 10 ? pct.toFixed(1) : Math.round(pct)}%`);
  } else if (rec) {
    rank.classList.remove("skel");
    setText("rank", "unranked", "v num flat");
    setText("rank-s", snap.fieldSize ? `field of ${fmt0.format(snap.fieldSize)}` : "");
  }
  renderSpark(rec?.history ?? []);
}

// ── sparkline (with a scrub cursor) ──────────────────────────────────────
const spark = { pts: [], x: null, y: null, drawn: false };
const NS = "http://www.w3.org/2000/svg";
const mk = (tag, attrs) => {
  const n = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
  return n;
};

function renderSpark(history) {
  const svg = $("spark");
  svg.replaceChildren();
  const pts = history.filter((p) => Number.isFinite(p.e) && p.e > 0);
  spark.pts = pts;
  const span = pts.length > 1 ? (pts.at(-1).t - pts[0].t) / 3_600_000 : 0;
  setText("spark-from", pts.length > 1 ? `${span >= 1 ? Math.round(span) + " h" : Math.round(span * 60) + " min"} ago` : "history appears after a few pushes");
  if (pts.length < 2) {
    setText("spark-range", "");
    $("spark-end").hidden = true;
    return;
  }
  const W = 400;
  const H = 76;
  const t0 = pts[0].t;
  const t1 = pts.at(-1).t;
  let lo = Math.min(...pts.map((p) => p.e));
  let hi = Math.max(...pts.map((p) => p.e));
  setText("spark-range", `${money(lo)} – ${money(hi)}`);
  const pad = Math.max((hi - lo) * 0.15, 1);
  lo -= pad;
  hi += pad;
  const x = (t) => (t1 === t0 ? W : ((t - t0) / (t1 - t0)) * W);
  const y = (e) => H - ((e - lo) / (hi - lo)) * H;
  spark.x = x;
  spark.y = y;
  const d = pts.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)},${y(p.e).toFixed(1)}`).join("");
  const line = mk("path", { class: "line", d });
  svg.append(mk("line", { class: "base", x1: 0, x2: W, y1: y(pts[0].e), y2: y(pts[0].e) }));
  if (START_EQUITY > lo && START_EQUITY < hi && Math.abs(START_EQUITY - pts[0].e) > pad) {
    svg.append(mk("line", { class: "base-100k", x1: 0, x2: W, y1: y(START_EQUITY), y2: y(START_EQUITY) }));
  }
  svg.append(mk("path", { class: "area", d: `${d}L${W},${H}L0,${H}Z` }), line);
  const last = pts.at(-1);
  placeSpot($("spark-end"), x(last.t), y(last.e));
  if (!spark.drawn && !reduced.matches) {
    spark.drawn = true;
    requestAnimationFrame(() => {
      // The stroke is non-scaling, so dashes are in screen pixels: measure the path as it is rendered.
      const r = svg.getBoundingClientRect();
      const sx = r.width / W;
      const sy = r.height / H;
      let len = 0;
      for (let i = 1; i < pts.length; i++) {
        len += Math.hypot((x(pts[i].t) - x(pts[i - 1].t)) * sx, (y(pts[i].e) - y(pts[i - 1].e)) * sy);
      }
      line.style.setProperty("--len", len);
      line.style.strokeDasharray = String(len);
      line.classList.add("draw");
    });
  }
}

/** Put an HTML dot over an SVG coordinate (the SVG is stretched, so circles drawn inside it would be too). */
function placeSpot(node, cx, cy) {
  node.hidden = false;
  node.style.left = `${(cx / 400) * 100}%`;
  node.style.top = `${(cy / 76) * 100}%`;
}

function wireScrub() {
  const svg = $("spark");
  const chart = $("chart");
  const box = $("scrub");
  const dot = $("spark-cursor");
  let cursor = null;
  let idx = -1;
  const hide = () => {
    box.hidden = true;
    dot.hidden = true;
    cursor?.remove();
    cursor = null;
    idx = -1;
  };
  const showAt = (best) => {
    if (!best || spark.pts.length < 2) return;
    idx = spark.pts.indexOf(best);
    const r = svg.getBoundingClientRect();
    const cx = spark.x(best.t);
    if (!cursor) {
      cursor = mk("line", { class: "cursor", x1: cx, x2: cx, y1: 0, y2: 76 });
      svg.append(cursor);
    }
    cursor.setAttribute("x1", cx);
    cursor.setAttribute("x2", cx);
    placeSpot(dot, cx, spark.y(best.e));
    const px = (cx / 400) * r.width;
    box.hidden = false;
    box.className = `scrub num${px < 70 ? " edge-l" : px > r.width - 70 ? " edge-r" : ""}`;
    box.style.left = `${px}px`;
    $("scrub-v").textContent = money(best.e);
    const delta = best.e - START_EQUITY;
    $("scrub-t").textContent = `${signed(delta)} · ${dayTime(best.t)}`;
    chart.setAttribute("aria-valuetext", `${money(best.e)}, ${signed(delta)}, ${dayTime(best.t)}`);
  };
  svg.addEventListener("pointermove", (ev) => {
    if (spark.pts.length < 2) return;
    const r = svg.getBoundingClientRect();
    const fx = Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width));
    const t = spark.pts[0].t + fx * (spark.pts.at(-1).t - spark.pts[0].t);
    let best = spark.pts[0];
    for (const p of spark.pts) if (Math.abs(p.t - t) < Math.abs(best.t - t)) best = p;
    showAt(best);
  });
  svg.addEventListener("pointerleave", hide);
  svg.addEventListener("pointercancel", hide);
  chart.addEventListener("keydown", (ev) => {
    if (spark.pts.length < 2) return;
    if (ev.key === "ArrowRight" || ev.key === "ArrowLeft") {
      ev.preventDefault();
      const next = ev.key === "ArrowRight"
        ? Math.min(spark.pts.length - 1, Math.max(0, idx) + 1)
        : Math.max(0, (idx < 0 ? spark.pts.length : idx) - 1);
      showAt(spark.pts[next]);
    } else if (ev.key === "Home") {
      ev.preventDefault();
      showAt(spark.pts[0]);
    } else if (ev.key === "End") {
      ev.preventDefault();
      showAt(spark.pts.at(-1));
    } else if (ev.key === "Escape") hide();
  });
  chart.addEventListener("blur", hide);
}

// ── health ───────────────────────────────────────────────────────────────
function renderHealth(items) {
  const box = $("health");
  box.replaceChildren();
  const bad = items.filter((h) => h.state !== "ok").length;
  setText("health-sum", items.length ? (bad ? `${bad} of ${items.length} need attention` : `all ${items.length} green`) : "");
  if (!items.length) {
    box.append(el("div", { class: "hi empty" }, "No health reported yet."));
    return;
  }
  const order = { down: 0, degraded: 1, ok: 2 };
  const rows = [...items]
    .sort((a, b) => (order[a.state] ?? 0) - (order[b.state] ?? 0) || a.name.localeCompare(b.name))
    .map((h) => {
      const last = nsToMs(h.lastOkNs);
      const detail = h.detail && h.detail !== h.state ? `${h.state} · ${h.detail}` : h.state;
      return el(
        "div",
        { class: `hi ${h.state}`, title: detail },
        el("span", { class: `hdot dot ${h.state === "ok" ? "ok" : ""}`, "aria-hidden": "true" }),
        el("span", { class: "hname" }, h.name),
        el("span", { class: "hwhen" }, last ? ago(state.serverNow - last).replace(" ago", "") : ""),
        el("span", { class: "hdetail", style: "grid-column: 2 / span 2" }, detail),
      );
    });
  box.append(...stagger(rows));
}

// ── positions ────────────────────────────────────────────────────────────
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
    box.append(el("div", { class: "empty" }, "No positions. ", el("em", {}, "Sitting very still in the woods.")));
    return;
  }
  const sorted = [...rows].sort((a, b) => Math.abs(b.unrealized ?? 0) - Math.abs(a.unrealized ?? 0));
  const maxU = Math.max(...sorted.map((p) => Math.abs(p.unrealized ?? 0)), 1e-9);
  const trs = sorted.map((p) => {
    const [side, q] = sideOf(p.quantity);
    const u = p.unrealized;
    const cost = p.avgCost !== undefined ? q * p.avgCost : null;
    return el(
      "tr",
      {},
      el("td", { class: "title" }, p.title || p.exchangeId || "?"),
      el("td", { class: "r num" }, el("span", { class: `side ${side}` }, side.toUpperCase()), fmt0.format(q)),
      el("td", { class: "r num dim hide-sm" }, cost === null ? "—" : money(cost)),
      el("td", { class: "r num dim hide-sm" }, p.avgCost?.toFixed(3) ?? "—"),
      el("td", { class: "r num hide-sm" }, p.mark?.toFixed(3) ?? "—"),
      el(
        "td",
        { class: `r num ${tone(u ?? 0)}` },
        el(
          "span",
          { class: "bar-cell" },
          u === undefined ? "—" : signed(u),
          u === undefined ? null : el("span", { class: `ubar ${tone(u)}` }, el("i", { style: `--w: ${(Math.abs(u) / maxU) * 0.5}` })),
        ),
      ),
    );
  });
  box.append(
    el(
      "div",
      { class: "table-wrap" },
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, "market"), el("th", { class: "r" }, "position"), el("th", { class: "r hide-sm" }, "cost"), el("th", { class: "r hide-sm" }, "avg"), el("th", { class: "r hide-sm" }, "mark"), el("th", { class: "r" }, "unrealized"))),
        el("tbody", {}, stagger(trs)),
      ),
    ),
  );
}

// ── trades ───────────────────────────────────────────────────────────────
function renderTrades(rows) {
  const box = $("trades");
  box.replaceChildren();
  const now = state.serverNow || Date.now();
  const today = rows.filter((t) => (nsToMs(t.tsNs) ?? 0) > now - 86_400_000).length;
  setText("trades-sum", rows.length ? `${today} in 24 h` : "");
  if (!rows.length) {
    box.append(el("div", { class: "empty" }, "No trades yet."));
    return;
  }
  const sorted = [...rows].sort((a, b) => (nsToMs(b.tsNs) ?? 0) - (nsToMs(a.tsNs) ?? 0)).slice(0, 12);
  const lis = sorted.map((t) => {
    const side = (t.side ?? "").replace("SIDE_", "").toLowerCase();
    const action = (t.action ?? "").replace("ACTION_", "").toLowerCase();
    const ms = nsToMs(t.tsNs);
    return el(
      "li",
      {},
      el("time", { datetime: ms ? new Date(ms).toISOString() : null }, ms ? time(ms) : "—"),
      el("span", { class: "who" }, t.strategy ?? "?"),
      el("span", { class: "what", title: t.title ?? "" }, t.title ?? ""),
      el("span", { class: `qty num ${action}` }, `${action} `, el("span", { class: `side ${side}` }, side.toUpperCase()), el("b", {}, fmt0.format(Number(t.quantity ?? 0))), ` @ ${t.price?.toFixed(3) ?? "—"}`),
    );
  });
  box.append(el("ul", { class: "tl" }, stagger(lis)));
}

// ── events ───────────────────────────────────────────────────────────────
function renderEvents(events) {
  const box = $("events");
  box.replaceChildren();
  const now = state.serverNow || Date.now();
  const recent = events.filter((e) => e.t > now - 86_400_000).reverse();
  const risky = recent.filter((e) => RISK.test(e.text)).length;
  setText("events-sum", recent.length ? `last 24 h · ${recent.length}${risky ? ` · ${risky} risk` : ""}` : "last 24 h");
  if (!recent.length) {
    box.append(el("li", {}, el("time", {}, ""), el("span", {}, "Quiet day. Nothing notable in the last 24 hours.")));
    return;
  }
  const lis = [];
  let lastDay = new Date(now).toDateString();
  for (const e of recent.slice(0, 40)) {
    const day = new Date(e.t).toDateString();
    if (day !== lastDay) {
      lis.push(el("li", { class: "day" }, el("span", {}, new Date(e.t).toLocaleDateString([], { weekday: "long" }))));
      lastDay = day;
    }
    lis.push(el("li", { class: RISK.test(e.text) ? "risk" : "" }, el("time", { datetime: new Date(e.t).toISOString() }, time(e.t)), el("span", {}, e.text)));
  }
  box.append(...stagger(lis, 20));
}

// ── today ────────────────────────────────────────────────────────────────
function renderSummary() {
  const s = state.summary;
  const snap = state.status?.snapshot ?? {};
  const items = [...(snap.actionNeeded ?? [])];
  for (const a of s?.actionNeeded ?? []) if (!items.includes(a) && !/^Server silent since/.test(a)) items.push(a);
  if (state.stale && state.status) items.unshift(`Server silent since ${dayTime(state.status.receivedAt)}. SSH in: make ps (restore runbook in docs/RUNBOOK.md).`);

  const action = $("action");
  action.replaceChildren();
  action.className = items.length ? "action" : "action clear";
  action.append(el("h3", {}, items.length ? "ACTION NEEDED" : "nothing needs you"));
  action.append(items.length ? el("ul", {}, items.map((i) => el("li", {}, i))) : el("p", {}, "No action today. The next summary lands at 14:00 UTC."));

  setText("summary-date", s ? new Date(`${s.date}T12:00:00Z`).toLocaleDateString([], { weekday: "short", month: "short", day: "numeric", timeZone: "UTC" }) : "no daily summary yet");
  const link = $("summary-link");
  link.replaceChildren();
  if (s?.posted?.url) link.append(el("a", { href: s.posted.url, target: "_blank", rel: "noopener noreferrer" }, "on GitHub ↗"));
  else if (s) link.append(`generated ${time(Date.parse(s.generatedAt))}`);

  const digest = $("digest");
  digest.replaceChildren();
  const row = (k, ...v) => el("div", { class: `digest-row ${["risk", "health"].includes(k) ? "" : "opt"}` }, el("span", { class: "k" }, k), el("span", { class: "v" }, ...v));
  if (!s) {
    digest.append(row("summary", "The daily Worker posts at 14:00 UTC. Live numbers are above."));
    return;
  }
  const h = s.headline;
  digest.append(
    row("24h", h.change24h === null ? "not enough history yet" : el("span", { class: `num ${tone(h.change24h)}` }, signed(h.change24h)), h.change24h === null || h.equity === null ? "" : ` to ${money(h.equity)}`),
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
  const out = el("form", { method: "post", action: "/api/logout", class: "signout" }, el("button", { type: "submit" }, "sign out"));
  $("footer").replaceChildren(...parts.map((p) => el("span", {}, p)), out);
}

function renderTitle(snap) {
  const bits = [];
  if (snap.killed) bits.push("KILLED");
  else if (state.stale) bits.push("silent");
  if (snap.equity !== undefined) bits.push(fmt0.format(snap.equity));
  if (snap.rank) bits.push(`#${snap.rank}`);
  document.title = bits.length ? `${bits.join(" · ")} · susquatch` : "Susquatch";
}

// ── kill (two steps, the second is a press-and-hold) ─────────────────────
function wireKill() {
  const dialog = $("kill-dialog");
  const reason = $("kill-reason");
  const confirm = $("kill-confirm");
  const label = confirm.querySelector("span");
  const err = $("kill-err");
  let hold = null;

  const cancelHold = () => {
    if (!hold) return;
    cancelAnimationFrame(hold.raf);
    hold = null;
    confirm.classList.remove("holding");
    confirm.style.setProperty("--p", 0);
  };
  const startHold = () => {
    if (confirm.disabled || hold) return;
    if (reduced.matches) return send();
    const t0 = performance.now();
    confirm.classList.add("holding");
    const step = (now) => {
      const p = Math.min(1, (now - t0) / HOLD_MS);
      confirm.style.setProperty("--p", p);
      if (p >= 1) {
        hold = null;
        confirm.classList.remove("holding");
        send();
      } else hold.raf = requestAnimationFrame(step);
    };
    hold = { raf: requestAnimationFrame(step) };
  };

  async function send() {
    confirm.disabled = true;
    label.textContent = "Sending…";
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
      label.textContent = "Hold to kill";
      confirm.style.setProperty("--p", 0);
    }
  }

  $("kill-open").addEventListener("click", () => {
    reason.value = "";
    confirm.disabled = true;
    err.hidden = true;
    $("kill-open").setAttribute("aria-expanded", "true");
    dialog.showModal();
    reason.focus();
  });
  $("kill-cancel").addEventListener("click", () => dialog.close());
  dialog.addEventListener("close", () => {
    cancelHold();
    $("kill-open").setAttribute("aria-expanded", "false");
    if (!$("kill-open").disabled) $("kill-open").focus();
  });
  reason.addEventListener("input", () => {
    confirm.disabled = reason.value.trim().length === 0;
  });
  confirm.addEventListener("pointerdown", (ev) => {
    ev.preventDefault();
    confirm.setPointerCapture?.(ev.pointerId);
    startHold();
  });
  for (const evt of ["pointerup", "pointercancel", "pointerleave"]) confirm.addEventListener(evt, cancelHold);
  confirm.addEventListener("keydown", (ev) => {
    if ((ev.key === " " || ev.key === "Enter") && !ev.repeat) {
      ev.preventDefault();
      startHold();
    }
  });
  confirm.addEventListener("keyup", (ev) => (ev.key === " " || ev.key === "Enter") && cancelHold());
  // The form never submits on its own: only a completed hold calls send().
  $("kill-form").addEventListener("submit", (ev) => ev.preventDefault());
}

// ── boot ─────────────────────────────────────────────────────────────────
wireKill();
wireScrub();
renderCalendar({});
refresh();
refreshSummary();
setInterval(() => document.visibilityState === "visible" && refresh(), REFRESH_MS);
setInterval(() => document.visibilityState === "visible" && refreshSummary(), SUMMARY_MS);
setInterval(() => {
  tickHeartbeat();
  renderCalendar(state.status?.snapshot ?? {});
}, 1000);
addEventListener("resize", () => positionMarker());
document.addEventListener("visibilitychange", () => document.visibilityState === "visible" && refresh());
