/**
 * Browser sessions for the login page: a signed, expiring cookie instead of the browser's Basic-auth prompt.
 *
 * The cookie is `<expiry-unix-seconds>.<hmac>` where the HMAC-SHA256 key is derived from DASHBOARD_PASSWORD, so
 * there is no second secret to manage and changing the password logs every browser out. Nothing is stored in KV:
 * verification is a pure computation, which keeps the free tier's write budget for status pushes.
 *
 * Cookie flags: HttpOnly (scripts can't read it), SameSite=Strict (never sent cross-site, so Kill can't be
 * ridden by another origin), Secure on https, Path=/. Basic and Bearer still work for curl, the pusher and tests.
 */
import { safeEqual } from "./auth.ts";

export const COOKIE = "susq_session";
export const SESSION_DAYS = 30;

const enc = new TextEncoder();

async function key(password: string): Promise<CryptoKey> {
  const raw = await crypto.subtle.digest("SHA-256", enc.encode(`${password}\nsusq-session-v1`));
  return crypto.subtle.importKey("raw", raw, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
}

function hex(buf: ArrayBuffer): string {
  return [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function sign(password: string, expires: number): Promise<string> {
  return hex(await crypto.subtle.sign("HMAC", await key(password), enc.encode(String(expires))));
}

/** A fresh session token that expires SESSION_DAYS from `now`. */
export async function mint(password: string, now = Date.now()): Promise<string> {
  const expires = Math.floor(now / 1000) + SESSION_DAYS * 86_400;
  return `${expires}.${await sign(password, expires)}`;
}

/** True when the token was signed with this password and has not expired. */
export async function verify(token: string | null | undefined, password: string, now = Date.now()): Promise<boolean> {
  if (!token) return false;
  const dot = token.indexOf(".");
  if (dot < 0) return false;
  const expires = Number(token.slice(0, dot));
  if (!Number.isInteger(expires) || expires * 1000 <= now) return false;
  return safeEqual(token.slice(dot + 1), await sign(password, expires));
}

export function readCookie(request: Request, name = COOKIE): string | null {
  for (const part of (request.headers.get("Cookie") ?? "").split(";")) {
    const eq = part.indexOf("=");
    if (eq > 0 && part.slice(0, eq).trim() === name) return part.slice(eq + 1).trim();
  }
  return null;
}

export function setCookie(token: string, secure: boolean): string {
  const flags = `Path=/; HttpOnly; SameSite=Strict; Max-Age=${SESSION_DAYS * 86_400}${secure ? "; Secure" : ""}`;
  return `${COOKIE}=${token}; ${flags}`;
}

export function clearCookie(secure: boolean): string {
  return `${COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0${secure ? "; Secure" : ""}`;
}

/** Only ever send people back to a local path: never to another origin. */
export function safeNext(value: string | null | undefined): string {
  if (!value || !value.startsWith("/") || value.startsWith("//") || value.startsWith("/login") || value.startsWith("/api/")) return "/";
  return value.length > 512 ? "/" : value;
}
