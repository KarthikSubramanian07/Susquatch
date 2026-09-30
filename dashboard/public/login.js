// Login page: carry `next` through, show the error flag, and count down to the open or the lock.
const q = new URLSearchParams(location.search);
const next = q.get("next") ?? "/";
if (next.startsWith("/") && !next.startsWith("//")) document.getElementById("next").value = next;
if (q.get("error")) {
  document.getElementById("err").hidden = false;
  document.body.classList.add("wrong");
  history.replaceState(null, "", location.pathname + (next !== "/" ? `?next=${encodeURIComponent(next)}` : ""));
}

document.querySelector("form").addEventListener("submit", (ev) => {
  const btn = ev.target.querySelector(".go");
  btn.disabled = true;
  btn.querySelector("span").textContent = "Checking…";
});

const OPEN = Date.parse("2026-10-01T16:00:00Z");
const LOCK = Date.parse("2026-11-04T17:00:00Z");
const clock = document.getElementById("clock");
function pad(n) {
  return String(n).padStart(2, "0");
}
function tick() {
  const now = Date.now();
  const [label, at] = now < OPEN ? ["opens in", OPEN] : now < LOCK ? ["locks in", LOCK] : [null, 0];
  if (!label) {
    clock.textContent = "locked · settling";
    return;
  }
  const s = Math.max(0, Math.floor((at - now) / 1000));
  const d = Math.floor(s / 86_400);
  const h = Math.floor((s % 86_400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  clock.replaceChildren(`${label} `, Object.assign(document.createElement("b"), { textContent: d > 0 ? `${d}d ${pad(h)}h ${pad(m)}m` : `${h}h ${pad(m)}m ${pad(s % 60)}s` }));
}
tick();
setInterval(tick, 1000);
