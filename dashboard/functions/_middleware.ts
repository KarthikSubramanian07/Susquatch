/**
 * Every request (static assets included) passes through here. Pages runs a root middleware for all routes.
 *
 * - /robots.txt is the only thing served without credentials (it says "go away").
 * - Human: HTTP Basic with DASHBOARD_PASSWORD. Server: Bearer SUSQ_STATUS_PUSH_TOKEN, machine routes only.
 * - Security headers on every response: noindex, no framing, strict CSP, no caching of private data.
 */
import { authenticate, configured, serverMayCall } from "../src/auth.ts";
import { type Fn, problem } from "../src/http.ts";

const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self' https://fonts.googleapis.com",
  "font-src https://fonts.gstatic.com",
  "img-src 'self' data:",
  "connect-src 'self'",
  "frame-ancestors 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join("; ");

function harden(res: Response, pathname: string): Response {
  const out = new Response(res.body, res);
  out.headers.set("X-Robots-Tag", "noindex, nofollow, noarchive");
  out.headers.set("X-Frame-Options", "DENY");
  out.headers.set("X-Content-Type-Options", "nosniff");
  out.headers.set("Referrer-Policy", "no-referrer");
  out.headers.set("Content-Security-Policy", CSP);
  out.headers.set("Permissions-Policy", "camera=(), microphone=(), geolocation=()");
  if (pathname !== "/robots.txt") out.headers.set("Cache-Control", "no-store");
  return out;
}

export const onRequest: Fn = async (ctx) => {
  const url = new URL(ctx.request.url);
  if (url.pathname === "/robots.txt") return harden(await ctx.next(), url.pathname);

  const ok = configured(ctx.env);
  if (!ok.human && !ok.server) {
    return harden(problem(503, "dashboard not configured: set DASHBOARD_PASSWORD and SUSQ_STATUS_PUSH_TOKEN"), url.pathname);
  }

  const principal = await authenticate(ctx.request, ctx.env);
  if (principal === null) {
    const challenge = { "WWW-Authenticate": 'Basic realm="susquatch", charset="UTF-8"' };
    return harden(problem(401, "authentication required", challenge), url.pathname);
  }
  if (principal === "server" && !serverMayCall(url.pathname, ctx.request.method)) {
    return harden(problem(403, "server token is limited to /api/push and /api/kill"), url.pathname);
  }
  ctx.data.principal = principal;
  return harden(await ctx.next(), url.pathname);
};
