/**
 * GitHub REST calls for the daily summary: find/create the pinned "Daily summary" issue, comment on it, and
 * find the latest weekly review report. The fine-grained token needs, on the private reports repo only:
 * Issues: read & write, and (optional, for the weekly link) Contents: read.
 */

export type Fetch = typeof fetch;

export const LABEL = "daily-summary";

export class GitHubError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

export class GitHub {
  private readonly repo: string;
  private readonly token: string;
  private readonly fetchImpl: Fetch;

  constructor(repo: string, token: string, fetchImpl?: Fetch) {
    this.repo = repo;
    this.token = token;
    // Never store the global `fetch` as a bare field: calling it as `this.fetchImpl(...)` binds `this` to the
    // GitHub instance and workerd throws "Illegal invocation" (that silently killed every cron run from
    // Oct 1 to Oct 2). The wrapper keeps the global receiver.
    // `run()` passes the bare global through as well, so wrap whatever we were given, not just the default.
    const impl = fetchImpl ?? fetch;
    this.fetchImpl = impl === fetch ? (input, init) => fetch(input, init) : impl;
  }

  private async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await this.fetchImpl(`https://api.github.com/repos/${this.repo}${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.token}`,
        Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "susquatch-summary-worker",
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok) throw new GitHubError(res.status, `GitHub ${method} ${path}: ${res.status}`);
    return (await res.json()) as T;
  }

  /** The open issue labelled `daily-summary`, created (once) if missing. Pin it by hand in the GitHub UI. */
  async summaryIssue(title: string): Promise<number> {
    const open = await this.req<{ number: number }[]>("GET", `/issues?state=open&labels=${LABEL}&per_page=1`);
    if (open[0]) return open[0].number;
    const created = await this.req<{ number: number }>("POST", "/issues", {
      title,
      labels: [LABEL],
      body: "One comment per day from the Susquatch summary Worker. Pin this issue and watch it to get the email.",
    });
    return created.number;
  }

  async comment(issue: number, body: string): Promise<string> {
    const res = await this.req<{ html_url: string }>("POST", `/issues/${issue}/comments`, { body });
    return res.html_url;
  }

  /** Newest `weekly/*.md` in the reports repo, or null (missing folder or no Contents permission). */
  async latestWeekly(): Promise<{ title: string; url: string } | null> {
    try {
      const items = await this.req<{ name: string; html_url: string; type: string }[]>("GET", "/contents/weekly");
      const md = items.filter((i) => i.type === "file" && i.name.endsWith(".md")).sort((a, b) => a.name.localeCompare(b.name));
      const last = md.at(-1);
      return last ? { title: last.name.replace(/\.md$/, ""), url: last.html_url } : null;
    } catch {
      return null; // the weekly link is decoration; no GitHub problem may stop the summary
    }
  }
}
