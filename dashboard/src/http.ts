/** Small response helpers shared by the Pages Functions. */

export interface Env {
  SUSQ_KV: KVNamespace;
  DASHBOARD_PASSWORD?: string;
  SUSQ_STATUS_PUSH_TOKEN?: string;
  STALE_AFTER_S?: string;
}

export interface Data extends Record<string, unknown> {
  principal?: "human" | "server";
}

export type Fn = PagesFunction<Env, string, Data>;

export function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store", ...headers },
  });
}

export function problem(status: number, message: string, headers: Record<string, string> = {}): Response {
  return json({ error: message }, status, headers);
}

export function staleAfterMs(env: Env): number {
  const s = Number(env.STALE_AFTER_S ?? "420");
  return (Number.isFinite(s) && s > 0 ? s : 420) * 1000;
}

/** Read the body with a hard size cap (Content-Length can lie or be absent). */
export async function readJson(request: Request, maxBytes: number): Promise<unknown> {
  const declared = Number(request.headers.get("Content-Length") ?? "0");
  if (declared > maxBytes) throw new RangeError("body too large");
  const buf = await request.arrayBuffer();
  if (buf.byteLength > maxBytes) throw new RangeError("body too large");
  return JSON.parse(new TextDecoder().decode(buf));
}
