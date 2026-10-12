#!/usr/bin/env node
// Render the 1200x630 link-preview card (public/og.png) with the system Chrome.
//   node scripts/og.mjs [out.png]
// The card is what LinkedIn, Slack and X show when someone pastes the dashboard link. It holds no private data.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { chromium } from "playwright-core";

const out = process.argv[2] ?? join(import.meta.dirname, "../public/og.png");
const logo = readFileSync(join(import.meta.dirname, "../public/favicon.svg"), "utf8");

const html = `<!doctype html><html><head><meta charset="utf-8">
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=DM+Sans:opsz,wght@9..40,400;9..40,500;9..40,600&family=JetBrains+Mono:wght@400;500&display=block">
<style>
  * { margin: 0; box-sizing: border-box; }
  html, body { width: 1200px; height: 630px; background: #09090b; overflow: hidden; }
  body { position: relative; font-family: "DM Sans", sans-serif; color: #fafafa; }
  .glow { position: absolute; inset: 0; background: radial-gradient(900px 420px at 78% 115%, rgba(217,119,87,.16), transparent 70%); }
  .grid { position: absolute; inset: 0; background-image: linear-gradient(rgba(255,255,255,.035) 1px, transparent 1px),
          linear-gradient(90deg, rgba(255,255,255,.035) 1px, transparent 1px); background-size: 60px 60px;
          mask-image: linear-gradient(180deg, transparent, #000 40%, #000 70%, transparent); }
  svg.curve { position: absolute; left: 0; right: 0; bottom: 0; width: 1200px; height: 300px; }
  .wrap { position: absolute; left: 88px; top: 92px; right: 88px; }
  .brand { display: flex; align-items: center; gap: 26px; }
  .brand svg { width: 104px; height: 104px; }
  h1 { font-size: 92px; font-weight: 600; letter-spacing: -0.045em; line-height: 1; }
  .tag { margin-top: 40px; font-size: 38px; line-height: 1.25; letter-spacing: -0.015em; color: #b4b4bc; max-width: 900px; }
  .tag b { color: #fafafa; font-weight: 500; }
  .meta { position: absolute; left: 88px; bottom: 70px; display: flex; gap: 14px; font-family: "JetBrains Mono", monospace; font-size: 21px; color: #7d7d87; }
  .meta span { padding: 9px 16px; border: 1px solid rgba(255,255,255,.1); border-radius: 999px; background: rgba(14,14,17,.8); }
  .meta span.hot { color: #d97757; border-color: rgba(217,119,87,.35); background: rgba(217,119,87,.08); }
</style></head><body>
  <div class="grid"></div><div class="glow"></div>
  <svg class="curve" viewBox="0 0 1200 300" preserveAspectRatio="none" aria-hidden="true">
    <path d="M0 296 C 520 296, 700 40, 905 40 S 1120 250, 1200 268" fill="none" stroke="#d97757" stroke-opacity=".55" stroke-width="3"/>
    <path d="M905 40 V 296" stroke="#d97757" stroke-opacity=".4" stroke-width="2" stroke-dasharray="3 7"/>
  </svg>
  <div class="wrap">
    <div class="brand">${logo}<h1>susquatch</h1></div>
    <p class="tag">A large, elusive trading system for the <b>Susquehanna Predictions Cup</b>. It sits very still in the woods, then comes out on <b>election night</b>.</p>
  </div>
  <div class="meta"><span class="hot">2026 midterms</span><span>rust</span><span>c++20</span><span>python</span><span>monte carlo</span></div>
</body></html>`;

const browser = await chromium.launch({ channel: "chrome" });
const page = await browser.newPage({ viewport: { width: 1200, height: 630 }, deviceScaleFactor: 1 });
await page.setContent(html, { waitUntil: "networkidle" });
await page.evaluate(() => document.fonts.ready);
await page.screenshot({ path: out, type: "png" });
await browser.close();
console.log(out);
