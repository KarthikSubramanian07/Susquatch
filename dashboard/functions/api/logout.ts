/** POST /api/logout: clear the session cookie and go back to the login page. */
import type { Fn } from "../../src/http.ts";
import { clearCookie } from "../../src/session.ts";

export const onRequestPost: Fn = async ({ request }) => {
  const url = new URL(request.url);
  const headers = new Headers({ Location: new URL("/login", url.origin).toString(), "Cache-Control": "no-store" });
  headers.append("Set-Cookie", clearCookie(url.protocol === "https:"));
  return new Response(null, { status: 303, headers });
};
