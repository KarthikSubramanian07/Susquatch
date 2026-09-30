import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { onRequest as middleware } from "../functions/_middleware.ts";
import * as login from "../functions/api/login.ts";
import * as logout from "../functions/api/logout.ts";
import { COOKIE, clearCookie, mint, readCookie, safeNext, setCookie, verify } from "../src/session.ts";
import { FakeKV, PASSWORD, call, makeEnv } from "./fakes.ts";

const U = "https://susquatch.pages.dev";
const env = makeEnv();
const page = async () => new Response("<html>");
const form = (body: Record<string, string>, extra: Record<string, string> = {}) => ({
  method: "POST",
  headers: { "Content-Type": "application/x-www-form-urlencoded", ...extra },
  body: new URLSearchParams(body).toString(),
});

describe("session tokens", () => {
  it("round-trip, expire, and reject tampering and other passwords", async () => {
    const t = await mint(PASSWORD, 1_000_000_000_000);
    assert.equal(await verify(t, PASSWORD, 1_000_000_000_000 + 1000), true);
    assert.equal(await verify(t, PASSWORD, 1_000_000_000_000 + 31 * 86_400_000), false, "expired after 30 days");
    assert.equal(await verify(t, `${PASSWORD}x`, 1_000_000_000_000), false, "another password");
    const [exp, mac] = t.split(".");
    assert.equal(await verify(`${Number(exp) + 86_400}.${mac}`, PASSWORD, 1_000_000_000_000), false, "forged expiry");
    assert.equal(await verify("garbage", PASSWORD), false);
    assert.equal(await verify(null, PASSWORD), false);
  });

  it("reads the cookie out of a header and writes the right flags", () => {
    const req = new Request(U, { headers: { Cookie: `a=1; ${COOKIE}=abc.def; b=2` } });
    assert.equal(readCookie(req), "abc.def");
    assert.equal(readCookie(new Request(U)), null);
    assert.match(setCookie("x", true), /^susq_session=x; Path=\/; HttpOnly; SameSite=Strict; Max-Age=2592000; Secure$/);
    assert.doesNotMatch(setCookie("x", false), /Secure/);
    assert.match(clearCookie(true), /Max-Age=0/);
  });

  it("only ever redirects to a local page", () => {
    assert.equal(safeNext("/"), "/");
    assert.equal(safeNext("/?x=1"), "/?x=1");
    assert.equal(safeNext("//evil.example"), "/");
    assert.equal(safeNext("https://evil.example"), "/");
    assert.equal(safeNext("/login?error=1"), "/");
    assert.equal(safeNext("/api/kill"), "/");
    assert.equal(safeNext(null), "/");
  });
});

describe("login flow", () => {
  it("sends a browser without a session to /login, remembering where it was going", async () => {
    const res = await call(page, { url: `${U}/`, headers: { Accept: "text/html,*/*" } }, env, { middleware });
    assert.equal(res.status, 302);
    assert.equal(res.headers.get("Location"), `${U}/login`);
    const deep = await call(page, { url: `${U}/some/page?x=1`, headers: { Accept: "text/html" } }, env, { middleware });
    assert.equal(new URL(deep.headers.get("Location")!).searchParams.get("next"), "/some/page?x=1");
  });

  it("keeps the 401 challenge for non-browser callers", async () => {
    const res = await call(page, { url: `${U}/api/status`, headers: { Accept: "application/json" } }, env, { middleware });
    assert.equal(res.status, 401);
    assert.match(res.headers.get("WWW-Authenticate") ?? "", /^Basic/);
  });

  it("serves the login page and its assets without credentials", async () => {
    for (const p of ["/login", "/login.css", "/login.js", "/favicon.svg"]) {
      const res = await call(page, { url: `${U}${p}` }, env, { middleware });
      assert.equal(res.status, 200, p);
      assert.equal(res.headers.get("Cache-Control"), "no-store");
      assert.equal(res.headers.get("X-Robots-Tag"), "noindex, nofollow, noarchive");
    }
  });

  it("sets a session on the right password and the session opens the dashboard", async () => {
    const res = await call(login.onRequestPost, { url: `${U}/api/login`, ...form({ password: PASSWORD, next: "/?tab=1" }) }, env, { middleware });
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("Location"), `${U}/?tab=1`);
    const cookie = res.headers.get("Set-Cookie") ?? "";
    assert.match(cookie, /^susq_session=\d+\.[0-9a-f]{64}; .*HttpOnly; SameSite=Strict; .*Secure$/);
    const token = /susq_session=([^;]+)/.exec(cookie)![1]!;
    const ui = await call(page, { url: `${U}/`, headers: { Accept: "text/html", Cookie: `susq_session=${token}` } }, env, { middleware });
    assert.equal(ui.status, 200);
    const api = await call(async (ctx) => new Response(String(ctx.data.principal)), { url: `${U}/api/status`, headers: { Cookie: `susq_session=${token}` } }, env, { middleware });
    assert.equal(await api.text(), "human");
    // A signed-in browser that hits /login is bounced to the dashboard.
    const again = await call(page, { url: `${U}/login`, headers: { Cookie: `susq_session=${token}` } }, env, { middleware });
    assert.equal(again.status, 302);
    assert.equal(again.headers.get("Location"), `${U}/`);
  });

  it("bounces a wrong password back with an error flag and no cookie", async () => {
    const t0 = Date.now();
    const res = await call(login.onRequestPost, { url: `${U}/api/login`, ...form({ password: "nope", next: "/x" }) }, env, { middleware });
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("Location"), `${U}/login?error=1&next=%2Fx`);
    assert.equal(res.headers.get("Set-Cookie"), null);
    assert.ok(Date.now() - t0 >= 500, "a wrong guess costs a moment");
  });

  it("refuses cross-origin, non-form and unconfigured logins", async () => {
    const xo = await call(login.onRequestPost, { url: `${U}/api/login`, ...form({ password: PASSWORD }, { Origin: "https://evil.example" }) }, env, { middleware });
    assert.equal(xo.status, 403);
    const json = await call(login.onRequestPost, { url: `${U}/api/login`, method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" }, env, { middleware });
    assert.equal(json.status, 415);
    const none = makeEnv(new FakeKV(), { DASHBOARD_PASSWORD: undefined });
    const res = await call(login.onRequestPost, { url: `${U}/api/login`, ...form({ password: "" }) }, none, { middleware });
    assert.equal(res.status, 503);
  });

  it("a session never unlocks the server-only routes, and logout clears it", async () => {
    const token = await mint(PASSWORD);
    const push = await call(async (ctx) => new Response(String(ctx.data.principal)), { url: `${U}/api/push`, method: "POST", headers: { Cookie: `susq_session=${token}` } }, env, { middleware });
    assert.equal(await push.text(), "human", "the push handler itself rejects humans with 403");
    const out = await call(logout.onRequestPost, { url: `${U}/api/logout`, method: "POST", headers: { Cookie: `susq_session=${token}` } }, env, { middleware });
    assert.equal(out.status, 303);
    assert.match(out.headers.get("Set-Cookie") ?? "", /Max-Age=0/);
    assert.equal(out.headers.get("Location"), `${U}/login`);
  });
});
