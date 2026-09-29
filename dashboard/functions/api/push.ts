/**
 * POST /api/push: the server's status pusher delivers a StatusSnapshot (server token only).
 *
 * Exactly one KV read and one KV write per accepted push. Pushes closer together than
 * LIMITS.minPushIntervalMs get 429 + Retry-After and cost no write, so a runaway pusher can't burn the
 * free tier's 1,000 writes/day.
 */
import { type Fn, json, problem, readJson } from "../../src/http.ts";
import { KEYS, LIMITS, type StatusRecord, ValidationError, applyPush, parseSnapshot } from "../../src/model.ts";

export const onRequestPost: Fn = async ({ request, env, data }) => {
  if (data.principal !== "server") return problem(403, "only the server pushes status");

  let snapshot;
  try {
    snapshot = parseSnapshot(await readJson(request, LIMITS.bodyBytes));
  } catch (err) {
    if (err instanceof RangeError) return problem(413, err.message);
    if (err instanceof ValidationError || err instanceof SyntaxError) return problem(400, err.message);
    throw err;
  }

  const now = Date.now();
  const prev = await env.SUSQ_KV.get<StatusRecord>(KEYS.status, "json");
  if (prev && now - prev.receivedAt < LIMITS.minPushIntervalMs) {
    const wait = Math.ceil((LIMITS.minPushIntervalMs - (now - prev.receivedAt)) / 1000);
    return problem(429, "pushing faster than the KV budget allows", { "Retry-After": String(wait) });
  }
  const record = applyPush(prev, snapshot, now);
  await env.SUSQ_KV.put(KEYS.status, JSON.stringify(record));
  return json({ ok: true, receivedAt: record.receivedAt, historyPoints: record.history.length });
};
