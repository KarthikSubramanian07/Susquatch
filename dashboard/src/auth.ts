/**
 * Who is calling: the human (HTTP Basic, DASHBOARD_PASSWORD) or the server (Bearer, SUSQ_STATUS_PUSH_TOKEN).
 *
 * - Secrets are compared in constant time: both sides are SHA-256'd and the digests compared with a
 *   full-length XOR, so neither the value nor its length leaks through timing.
 * - Misconfiguration fails closed: a missing or too-short secret disables that principal entirely (503 for
 *   everything if both are unusable). There is no default password and no "dev bypass".
 * - The server token only opens the two machine routes. It can never load the UI or press Kill.
 */

export interface AuthEnv {
  DASHBOARD_PASSWORD?: string;
  SUSQ_STATUS_PUSH_TOKEN?: string;
}

export type Principal = "human" | "server";

export const MIN_PASSWORD_LEN = 16;
export const MIN_TOKEN_LEN = 32;

/** Routes (path + method) the server's bearer token may call. Everything else is human-only. */
const SERVER_ROUTES: ReadonlyArray<readonly [string, string]> = [
  ["/api/push", "POST"],
  ["/api/kill", "GET"],
  ["/api/kill", "DELETE"],
];

async function digest(text: string): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)));
}

/** Constant-time string equality (for secrets). */
export async function safeEqual(a: string, b: string): Promise<boolean> {
  const [da, db] = await Promise.all([digest(a), digest(b)]);
  let diff = 0;
  for (let i = 0; i < da.length; i++) diff |= da[i]! ^ db[i]!;
  return diff === 0;
}

export function configured(env: AuthEnv): { human: boolean; server: boolean } {
  return {
    human: (env.DASHBOARD_PASSWORD ?? "").length >= MIN_PASSWORD_LEN,
    server: (env.SUSQ_STATUS_PUSH_TOKEN ?? "").length >= MIN_TOKEN_LEN,
  };
}

function decodeBasic(value: string): string | null {
  try {
    const decoded = atob(value);
    const colon = decoded.indexOf(":");
    return colon < 0 ? null : decoded.slice(colon + 1); // any username; the password is the secret
  } catch {
    return null;
  }
}

/** Resolve the caller, or null if the credentials are absent or wrong. */
export async function authenticate(request: Request, env: AuthEnv): Promise<Principal | null> {
  const header = request.headers.get("Authorization") ?? "";
  const ok = configured(env);
  const space = header.indexOf(" ");
  if (space < 0) return null;
  const scheme = header.slice(0, space).toLowerCase();
  const value = header.slice(space + 1).trim();
  if (scheme === "bearer" && ok.server) {
    return (await safeEqual(value, env.SUSQ_STATUS_PUSH_TOKEN!)) ? "server" : null;
  }
  if (scheme === "basic" && ok.human) {
    const password = decodeBasic(value);
    return password !== null && (await safeEqual(password, env.DASHBOARD_PASSWORD!)) ? "human" : null;
  }
  return null;
}

export function serverMayCall(pathname: string, method: string): boolean {
  return SERVER_ROUTES.some(([p, m]) => p === pathname && m === method.toUpperCase());
}
