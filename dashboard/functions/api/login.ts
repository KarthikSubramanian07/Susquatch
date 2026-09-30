/**
 * POST /api/login: the login page's form. A correct password sets the session cookie and redirects to `next`;
 * a wrong one waits a moment (a small brake on guessing) and returns to /login with an error flag.
 * Reachable without credentials; everything it can do is decided by the password alone.
 */
import { configured, safeEqual } from "../../src/auth.ts";
import { type Fn, problem } from "../../src/http.ts";
import { mint, safeNext, setCookie } from "../../src/session.ts";

const WRONG_DELAY_MS = 600;

export const onRequestPost: Fn = async ({ request, env }) => {
  if (!configured(env).human) return problem(503, "dashboard not configured: set DASHBOARD_PASSWORD");
  const url = new URL(request.url);
  const origin = request.headers.get("Origin");
  if (origin !== null && origin !== url.origin) return problem(403, "cross-origin login refused");
  const type = request.headers.get("Content-Type") ?? "";
  if (!type.startsWith("application/x-www-form-urlencoded") && !type.startsWith("multipart/form-data")) {
    return problem(415, "form submission required");
  }
  const form = await request.formData();
  const password = String(form.get("password") ?? "");
  const next = safeNext(String(form.get("next") ?? "/"));
  const secure = url.protocol === "https:";

  if (password.length === 0 || !(await safeEqual(password, env.DASHBOARD_PASSWORD!))) {
    await new Promise((r) => setTimeout(r, WRONG_DELAY_MS));
    const back = new URL("/login", url.origin);
    back.searchParams.set("error", "1");
    if (next !== "/") back.searchParams.set("next", next);
    return Response.redirect(back.toString(), 303);
  }
  const headers = new Headers({ Location: new URL(next, url.origin).toString(), "Cache-Control": "no-store" });
  headers.append("Set-Cookie", setCookie(await mint(env.DASHBOARD_PASSWORD!), secure));
  return new Response(null, { status: 303, headers });
};
