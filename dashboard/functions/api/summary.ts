/** GET /api/summary: the latest daily summary written by the cron Worker (human only). */
import { type Fn, json, problem } from "../../src/http.ts";
import { KEYS, type Summary } from "../../src/model.ts";

export const onRequestGet: Fn = async ({ env, data }) => {
  if (data.principal !== "human") return problem(403, "human only");
  const summary = await env.SUSQ_KV.get<Summary>(KEYS.summaryLatest, "json");
  return json({ summary });
};
