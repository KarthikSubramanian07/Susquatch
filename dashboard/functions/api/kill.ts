/**
 * The remote Kill switch.
 *
 *   GET    /api/kill  server (kill poller, every ~10 s) or human: the current flag
 *   POST   /api/kill  human only, the UI's confirmed second step: sets the flag with a reason
 *   DELETE /api/kill  server only: `make unkill`, run by a human over SSH, clears the flag
 *
 * POST requires JSON, an explicit intent header and a same-origin Origin (when the browser sends one).
 * Together these stop a cross-site form from riding the browser's cached Basic credentials.
 * Un-killing is deliberately not possible from the browser.
 */
import { type Fn, json, problem, readJson } from "../../src/http.ts";
import { KEYS, type KillRecord, ValidationError, parseKill } from "../../src/model.ts";

export const onRequestGet: Fn = async ({ env }) => {
  const kill = await env.SUSQ_KV.get<KillRecord>(KEYS.kill, "json");
  return json(kill ?? { killed: false });
};

export const onRequestPost: Fn = async ({ request, env, data }) => {
  if (data.principal !== "human") return problem(403, "only a human presses Kill");
  const origin = request.headers.get("Origin");
  if (origin !== null && origin !== new URL(request.url).origin) return problem(403, "cross-origin kill refused");
  if (request.headers.get("X-Susq-Intent") !== "kill") return problem(400, "missing X-Susq-Intent: kill");
  if (!(request.headers.get("Content-Type") ?? "").startsWith("application/json")) {
    return problem(415, "application/json required");
  }
  let reason: string;
  try {
    ({ reason } = parseKill(await readJson(request, 4096)));
  } catch (err) {
    if (err instanceof ValidationError || err instanceof SyntaxError || err instanceof RangeError) {
      return problem(400, err.message);
    }
    throw err;
  }
  const existing = await env.SUSQ_KV.get<KillRecord>(KEYS.kill, "json");
  if (existing?.killed) return json(existing); // already killed: keep the first reason, spend no write
  const record: KillRecord = { killed: true, reason, at: new Date().toISOString(), by: "dashboard" };
  await env.SUSQ_KV.put(KEYS.kill, JSON.stringify(record));
  return json(record, 201);
};

export const onRequestDelete: Fn = async ({ env, data }) => {
  if (data.principal !== "server") return problem(403, "un-kill runs on the server: make unkill");
  await env.SUSQ_KV.delete(KEYS.kill);
  return json({ killed: false });
};
