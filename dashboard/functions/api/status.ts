/** GET /api/status: everything the dashboard renders in one read-only call (human only). */
import { type Fn, json, problem, staleAfterMs } from "../../src/http.ts";
import { KEYS, type KillRecord, type StatusRecord, heartbeatAge, isStale } from "../../src/model.ts";

export const onRequestGet: Fn = async ({ env, data }) => {
  if (data.principal !== "human") return problem(403, "human only");
  const [status, kill] = await Promise.all([
    env.SUSQ_KV.get<StatusRecord>(KEYS.status, "json"),
    env.SUSQ_KV.get<KillRecord>(KEYS.kill, "json"),
  ]);
  const now = Date.now();
  const staleMs = staleAfterMs(env);
  const age = heartbeatAge(status, now);
  return json({
    now,
    status,
    kill: kill ?? { killed: false },
    heartbeatAgeMs: Number.isFinite(age) ? age : null,
    staleAfterMs: staleMs,
    stale: isStale(status, now, staleMs),
  });
};
