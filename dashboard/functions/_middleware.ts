/**
 * Every request (static assets included) passes through here. Pages runs a root middleware for all routes.
 *
 * - Public without credentials: /robots.txt (it says "go away"), the login page and its two assets, the favicon,
 *   and POST /api/login. Everything else needs a principal.
 * - Human: the session cookie set by the login page, or HTTP Basic with DASHBOARD_PASSWORD (curl, tests).
 *   Server: Bearer SUSQ_STATUS_PUSH_TOKEN, machine routes only.
 * - A browser navigating without credentials is sent to /login (remembering where it was going). Anything else
 *   gets a 401 with a Basic challenge, so scripts keep working.
 * - Security headers on every response: noindex, no framing, strict CSP, no caching of private data.
 */
import { authenticate, configured, serverMayCall } from "../src/auth.ts";
import { type Fn, problem } from "../src/http.ts";
import { readCookie, verify } from "../src/session.ts";

const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self' https://fonts.googleapis.com",
  "font-src https://fonts.gstatic.com",
  "img-src 'self' data:",
  "connect-src 'self'",
  "frame-ancestors 'none'",
  "base-uri 'none'",
  "form-action 'self'",
].join("; ");

const PUBLIC = new Set(["/robots.txt", "/login", "/login.html", "/login.css", "/login.js", "/favicon.svg"]);

function harden(res: Response, pathname: string): Response {
  const out = new Response(res.body, res);
  out.headers.set("X-Robots-Tag", "noindex, nofollow, noarchive");
  out.headers.set("X-Frame-Options", "DENY");
  out.headers.set("X-Content-Type-Options", "nosniff");
  out.headers.set("Referrer-Policy", "same-origin"); // not no-referrer: that makes browsers send Origin: null on POSTs
  out.headers.set("Content-Security-Policy", CSP);
  out.headers.set("Permissions-Policy", "camera=(), microphone=(), geolocation=()");
  if (pathname !== "/robots.txt") out.headers.set("Cache-Control", "no-store");
  return out;
}

function wantsPage(request: Request): boolean {
  return request.method === "GET" && (request.headers.get("Accept") ?? "").includes("text/html");
}

export const onRequest: Fn = async (ctx) => {
  const url = new URL(ctx.request.url);
  const path = url.pathname;
  if (PUBLIC.has(path) || (path === "/api/login" && ctx.request.method === "POST")) {
    // A browser that already has a session shouldn't see the login page again.
    if ((path === "/login" || path === "/login.html") && configured(ctx.env).human) {
      if (await verify(readCookie(ctx.request), ctx.env.DASHBOARD_PASSWORD!)) {
        return harden(Response.redirect(new URL("/", url.origin).toString(), 302), path);
      }
    }
    return harden(await ctx.next(), path);
  }

  const ok = configured(ctx.env);
  if (!ok.human && !ok.server) {
    return harden(problem(503, "dashboard not configured: set DASHBOARD_PASSWORD and SUSQ_STATUS_PUSH_TOKEN"), path);
  }

  let principal = await authenticate(ctx.request, ctx.env);
  if (principal === null && ok.human && !ctx.request.headers.has("Authorization")) {
    if (await verify(readCookie(ctx.request), ctx.env.DASHBOARD_PASSWORD!)) principal = "human";
  }
  if (principal === null) {
    if (wantsPage(ctx.request) && !ctx.request.headers.has("Authorization")) {
      const login = new URL("/login", url.origin);
      if (path !== "/") login.searchParams.set("next", path + url.search);
      return harden(Response.redirect(login.toString(), 302), path);
    }
    const challenge = { "WWW-Authenticate": 'Basic realm="susquatch", charset="UTF-8"' };
    return harden(problem(401, "authentication required", challenge), path);
  }
  if (principal === "server" && !serverMayCall(path, ctx.request.method)) {
    return harden(problem(403, "server token is limited to /api/push and /api/kill"), path);
  }
  ctx.data.principal = principal;
  return harden(await ctx.next(), path);
};
