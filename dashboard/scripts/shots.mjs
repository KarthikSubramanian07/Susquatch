#!/usr/bin/env node
// Screenshot the local dashboard (npm run dev must be running) with the system Chrome.
//   node scripts/shots.mjs <out-dir> [name-prefix]
// Writes <prefix>desktop.png (1440 wide, full page) and <prefix>mobile.png (390x844 first screen, 3x).
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { chromium } from "playwright-core";

const out = process.argv[2] ?? join(import.meta.dirname, "../../docs/img");
const prefix = process.argv[3] ?? "dashboard-";
const password = /DASHBOARD_PASSWORD=(.*)/.exec(readFileSync(join(import.meta.dirname, "../.dev.vars"), "utf8"))[1];
const url = process.env.DASH_URL ?? "http://localhost:8788/";

const browser = await chromium.launch({ channel: "chrome" });
const shots = [
  { name: "desktop", viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2, fullPage: true },
  { name: "mobile", viewport: { width: 390, height: 844 }, deviceScaleFactor: 3, isMobile: true, hasTouch: true, fullPage: false },
];
// EXTRA=1 also shoots the phone's first screen and the kill dialog, for design review.
if (process.env.EXTRA) {
  const phone = { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true, fullPage: false };
  shots.push({ ...phone, name: "mobile-fold" }, { ...phone, name: "mobile-kill", kill: true });
  shots.push({ name: "desktop-kill", viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1, fullPage: false, kill: true });
}
for (const s of shots) {
  const ctx = await browser.newContext({
    viewport: s.viewport,
    deviceScaleFactor: s.deviceScaleFactor,
    isMobile: s.isMobile,
    hasTouch: s.hasTouch,
    httpCredentials: { username: "susq", password },
    colorScheme: "dark",
    reducedMotion: "reduce", // deterministic frames: tweens and draw-ins collapse to their final state
  });
  const page = await ctx.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  await page.goto(url, { waitUntil: "networkidle" });
  await page.waitForFunction(() => document.getElementById("equity")?.textContent !== "—" || document.getElementById("silent")?.hidden === false);
  await page.waitForFunction(() => document.body.dataset.ready === "1");
  await page.evaluate(() => document.fonts.ready);
  if (s.kill && !(await page.isEnabled("#kill-open"))) {
    await ctx.close();
    continue; // already killed: the button is (correctly) disabled
  }
  if (s.kill) {
    await page.click("#kill-open");
    await page.fill("#kill-reason", "needle posteriors look inverted on ME-02");
    await page.waitForTimeout(250);
  }
  const path = join(out, `${prefix}${s.name}.png`);
  await page.screenshot({ path, fullPage: s.fullPage });
  console.log(path, errors.length ? `errors: ${errors.join(" | ")}` : "");
  await ctx.close();
}
await browser.close();
