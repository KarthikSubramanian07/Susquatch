/** Test doubles: an in-memory KV that counts writes, and a way to call Pages Functions directly. */
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { Env } from "../src/http.ts";

/** The golden StatusSnapshot fixture from contracts/ (the proto's JSON mapping), as text. */
export const fixtureText: string = readFileSync(
  join(import.meta.dirname, "../../contracts/fixtures/status_snapshot.json"),
  "utf8",
);

export class FakeKV {
  store = new Map<string, string>();
  writes = 0;
  reads = 0;

  async get(key: string, type?: "text" | "json" | { type: string }): Promise<unknown> {
    this.reads++;
    const v = this.store.get(key);
    if (v === undefined) return null;
    const kind = typeof type === "object" ? type.type : type;
    return kind === "json" ? JSON.parse(v) : v;
  }

  async put(key: string, value: string): Promise<void> {
    this.writes++;
    this.store.set(key, value);
  }

  async delete(key: string): Promise<void> {
    this.writes++;
    this.store.delete(key);
  }

  async list(opts: { prefix?: string } = {}): Promise<{ keys: { name: string }[]; list_complete: boolean }> {
    const keys = [...this.store.keys()].filter((k) => k.startsWith(opts.prefix ?? "")).sort();
    return { keys: keys.map((name) => ({ name })), list_complete: true };
  }

  json<T>(key: string): T | null {
    const v = this.store.get(key);
    return v === undefined ? null : (JSON.parse(v) as T);
  }
}

export const PASSWORD = "correct-horse-battery-staple";
export const TOKEN = "t".repeat(40);

export function makeEnv(kv = new FakeKV(), overrides: Partial<Env> = {}): Env & { SUSQ_KV: FakeKV } {
  return {
    SUSQ_KV: kv as unknown as KVNamespace,
    DASHBOARD_PASSWORD: PASSWORD,
    SUSQ_STATUS_PUSH_TOKEN: TOKEN,
    STALE_AFTER_S: "420",
    ...overrides,
  } as Env & { SUSQ_KV: FakeKV };
}

export const basic = (password = PASSWORD) => `Basic ${btoa(`susq:${password}`)}`;
export const bearer = (token = TOKEN) => `Bearer ${token}`;

type AnyFn = (ctx: any) => Response | Promise<Response>;

/** Invoke a Pages Function (optionally behind the middleware) the way the runtime would. */
export async function call(
  handler: AnyFn,
  init: { url?: string; method?: string; headers?: Record<string, string>; body?: string },
  env: Env,
  opts: { middleware?: AnyFn; principal?: "human" | "server" } = {},
): Promise<Response> {
  const request = new Request(init.url ?? "https://susquatch.pages.dev/api/x", {
    method: init.method ?? "GET",
    headers: init.headers,
    body: init.body,
  });
  const data: Record<string, unknown> = opts.principal ? { principal: opts.principal } : {};
  const base = { request, env, data, params: {}, waitUntil() {}, passThroughOnException() {}, functionPath: "" };
  const inner = () => handler({ ...base, next: async () => new Response("asset") });
  if (!opts.middleware) return inner();
  return opts.middleware({ ...base, next: inner });
}
