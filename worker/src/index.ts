/**
 * susquatch-summary: a Cloudflare Worker with one daily cron (14:00 UTC = 07:00 PT).
 *
 * Reads the status record and kill flag from KV, composes the summary, stores it for the dashboard
 * (`summary:latest` + `summary:YYYY-MM-DD`) and, unless PUSH_TO_GITHUB is "false", comments it on the pinned
 * "Daily summary" issue in the private reports repo (GitHub then emails the owner: no alert system needed).
 *
 * It runs on Cloudflare, not on the server, so when the server is dead the summary says so ("server silent
 * since ..."). That's the dead-man switch. Re-running the same day never posts twice.
 */
import { KEYS, type KillRecord, type StatusRecord, type Summary } from "../../dashboard/src/model.ts";
import { type Fetch, GitHub } from "./github.ts";
import { compose } from "./summary.ts";

export interface Env {
  SUSQ_KV: KVNamespace;
  GITHUB_TOKEN?: string;
  PUSH_TO_GITHUB?: string;
  REPORTS_REPO?: string;
  SUMMARY_ISSUE_TITLE?: string;
  DEADMAN_MINUTES?: string;
  DASHBOARD_URL?: string;
}

const KEEP_DAYS = 60;

export async function run(env: Env, now: Date, fetchImpl: Fetch = fetch): Promise<Summary> {
  const [status, kill] = await Promise.all([
    env.SUSQ_KV.get<StatusRecord>(KEYS.status, "json"),
    env.SUSQ_KV.get<KillRecord>(KEYS.kill, "json"),
  ]);
  const push = (env.PUSH_TO_GITHUB ?? "true").toLowerCase() !== "false";
  const gh = env.GITHUB_TOKEN && env.REPORTS_REPO ? new GitHub(env.REPORTS_REPO, env.GITHUB_TOKEN, fetchImpl) : null;

  const summary = compose(status, kill, {
    now,
    deadmanMinutes: Number(env.DEADMAN_MINUTES ?? "20") || 20,
    dashboardUrl: env.DASHBOARD_URL ?? "https://susquatch.pages.dev",
    weeklyReview: gh ? await gh.latestWeekly() : null,
  });

  const dayKey = KEYS.summaryDay(summary.date);
  const earlier = await env.SUSQ_KV.get<Summary>(dayKey, "json");
  if (earlier?.posted) {
    summary.posted = earlier.posted; // a retried/duplicate cron run never comments twice
  } else if (push && gh) {
    try {
      const issue = await gh.summaryIssue(env.SUMMARY_ISSUE_TITLE ?? "Daily summary");
      summary.posted = { url: await gh.comment(issue, summary.markdown) };
    } catch (err) {
      summary.actionNeeded.push(
        `Couldn't post today's summary to GitHub (${err instanceof Error ? err.message : String(err)}). ` +
          "Check the Worker's GITHUB_TOKEN (Issues: read & write on the reports repo).",
      );
    }
  } else if (push && !gh) {
    summary.actionNeeded.push("GitHub posting is on but GITHUB_TOKEN / REPORTS_REPO aren't set on the Worker.");
  }

  const body = JSON.stringify(summary);
  await Promise.all([
    env.SUSQ_KV.put(dayKey, body, { expirationTtl: KEEP_DAYS * 86_400 }),
    env.SUSQ_KV.put(KEYS.summaryLatest, body),
  ]);
  return summary;
}

export default {
  async scheduled(controller, env, ctx) {
    ctx.waitUntil(run(env, new Date(controller.scheduledTime)));
  },
  // No routes are bound (workers_dev = false); anything that does reach it gets nothing.
  async fetch() {
    return new Response("not found", { status: 404 });
  },
} satisfies ExportedHandler<Env>;
