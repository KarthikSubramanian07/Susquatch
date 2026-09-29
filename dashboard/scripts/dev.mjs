#!/usr/bin/env node
// Local dashboard: `npm run dev` (or `make dashboard-dev`).
// Creates dashboard/.dev.vars (gitignored) with random secrets on first run, seeds local KV unless
// SEED=none, then runs `wrangler pages dev` on http://localhost:8788 against LOCAL KV only.
import { spawn, execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dirname, "..");
const vars = join(root, ".dev.vars");
if (!existsSync(vars)) {
  const password = randomBytes(12).toString("base64url");
  const token = randomBytes(24).toString("base64url");
  writeFileSync(vars, `DASHBOARD_PASSWORD=${password}\nSUSQ_STATUS_PUSH_TOKEN=${token}\n`, { mode: 0o600 });
}
const password = /DASHBOARD_PASSWORD=(.*)/.exec(readFileSync(vars, "utf8"))?.[1];

const seed = process.env.SEED ?? "normal";
if (seed !== "none") execFileSync("node", [join(root, "scripts/seed.mjs"), seed], { cwd: root, stdio: "inherit" });

console.log(`\n  dashboard: http://localhost:8788   user: anything   password: ${password}   (from dashboard/.dev.vars)\n`);
const child = spawn(
  "npx",
  ["wrangler", "pages", "dev", "public", "--port", process.env.PORT ?? "8788", "--persist-to", ".wrangler/state"],
  { cwd: root, stdio: "inherit" },
);
child.on("exit", (code) => process.exit(code ?? 0));
