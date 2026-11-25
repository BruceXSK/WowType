// node --test relay/cloudflare/test/
import assert from "node:assert/strict";
import test from "node:test";
import { limits } from "../src/index.js";
import { call, makeEnv } from "./harness.mjs";

const CHANNEL = "/v1/c/" + "ab".repeat(16);
const b64 = (text) => Buffer.from(text).toString("base64");

test("根路径不用口令，别的都要", async () => {
  const env = makeEnv();
  assert.deepEqual(await (await call(env, "GET", "/", { token: null })).json(), {
    service: "wowtype-sync-relay",
    version: 1,
  });
  assert.equal((await call(env, "GET", "/v1/ping", { token: null })).status, 401);
  assert.equal((await call(env, "GET", "/v1/ping", { token: "wrong" })).status, 401);
  assert.equal((await call(env, "GET", "/v1/ping", { token: "secre" })).status, 401);
  assert.equal((await call(env, "GET", "/v1/ping", { token: "secretx" })).status, 401);
  assert.deepEqual(await (await call(env, "GET", "/v1/ping")).json(), { ok: true });
  assert.equal((await call(env, "POST", CHANNEL, { token: "wrong", body: b64("x") })).status, 401);
  assert.equal((await call(env, "GET", "/v1/elsewhere")).status, 404);
  assert.equal((await call(env, "GET", "/v1/c/short")).status, 404);
});

test("没设口令的部署什么都不给", async () => {
  const env = makeEnv("");
  assert.equal((await call(env, "GET", "/v1/ping", { token: "" })).status, 503);
  assert.equal((await call(env, "POST", CHANNEL, { token: "", body: b64("x") })).status, 503);
});

test("发、取、从哪条开始", async () => {
  const env = makeEnv();
  assert.deepEqual(await (await call(env, "GET", CHANNEL)).json(), { seq: 0, items: [] });
  assert.deepEqual(await (await call(env, "POST", CHANNEL, { body: b64("一") })).json(), { seq: 1 });
  assert.deepEqual(await (await call(env, "POST", CHANNEL, { body: b64("二") })).json(), { seq: 2 });
  // 新设备不带 after：只拿到现在的 seq，不翻旧账
  assert.deepEqual(await (await call(env, "GET", CHANNEL)).json(), { seq: 2, items: [] });
  assert.deepEqual(await (await call(env, "GET", CHANNEL + "?after=0")).json(), {
    seq: 2,
    items: [b64("一"), b64("二")],
  });
  assert.deepEqual(await (await call(env, "GET", CHANNEL + "?after=1")).json(), { seq: 2, items: [b64("二")] });
  assert.deepEqual(await (await call(env, "GET", CHANNEL + "?after=2")).json(), { seq: 2, items: [] });
  // 手里的 seq 比服务上的大（存储被清过）：从头给
  assert.equal((await (await call(env, "GET", CHANNEL + "?after=99")).json()).items.length, 2);
  assert.equal((await call(env, "GET", CHANNEL + "?after=x")).status, 400);
  // 别的信道看不到
  const other = "/v1/c/" + "cd".repeat(16);
  assert.deepEqual(await (await call(env, "GET", other + "?after=0")).json(), { seq: 0, items: [] });
});

test("正文只认不太长的 Base64", async () => {
  const env = makeEnv();
  assert.equal((await call(env, "POST", CHANNEL, { body: "" })).status, 400);
  assert.equal((await call(env, "POST", CHANNEL, { body: "不是 base64" })).status, 400);
  assert.equal((await call(env, "POST", CHANNEL, { body: "A".repeat(limits.MAX_BODY + 4) })).status, 400);
  assert.equal((await call(env, "POST", CHANNEL, { body: "A".repeat(limits.MAX_BODY) })).status, 200);
  assert.equal((await call(env, "PUT", CHANNEL, { body: b64("x") })).status, 405);
});

test("只留最近的几条，过期的删掉", async (t) => {
  const env = makeEnv();
  for (let i = 1; i <= limits.KEEP + 5; i++) await call(env, "POST", CHANNEL, { body: b64("第" + i) });
  const all = await (await call(env, "GET", CHANNEL + "?after=0")).json();
  assert.equal(all.seq, limits.KEEP + 5);
  assert.equal(all.items.length, limits.KEEP);
  assert.equal(all.items[0], b64("第6"));
  const storage = env.objects.get("c:" + "ab".repeat(16)).storage;
  assert.equal((await storage.list({ prefix: "i:" })).size, limits.KEEP);

  t.mock.timers.enable({ apis: ["Date"], now: Date.now() + limits.TTL_MS + 1000 });
  assert.deepEqual((await (await call(env, "GET", CHANNEL + "?after=0")).json()).items, []);
  assert.equal((await storage.list({ prefix: "i:" })).size, 0, "过期的从存储里删掉");
  // seq 接着往上走，不回头
  assert.equal((await (await call(env, "POST", CHANNEL, { body: b64("新的") })).json()).seq, limits.KEEP + 6);
});

test("长轮询：有新的马上回，没有等到时间", async () => {
  const env = makeEnv();
  await call(env, "POST", CHANNEL, { body: b64("旧的") });
  const started = Date.now();
  const waiting = call(env, "GET", CHANNEL + "?after=1&wait=20").then((r) => r.json());
  await new Promise((resolve) => setTimeout(resolve, 50));
  await call(env, "POST", CHANNEL, { body: b64("新的") });
  assert.deepEqual(await waiting, { seq: 2, items: [b64("新的")] });
  assert.ok(Date.now() - started < 5000, "有新的应该马上回");

  const before = Date.now();
  assert.deepEqual(await (await call(env, "GET", CHANNEL + "?after=2&wait=0.3")).json(), { seq: 2, items: [] });
  assert.ok(Date.now() - before >= 250, "没有新的要等满");
});

test("配对留言：各取各的，取走就没了", async () => {
  const env = makeEnv();
  assert.equal((await call(env, "GET", "/v1/pair/host")).status, 204);
  await call(env, "POST", "/v1/pair/host", { body: b64("给主持的 1") });
  await call(env, "POST", "/v1/pair/host", { body: b64("给主持的 2") });
  await call(env, "POST", "/v1/pair/join", { body: b64("给加入的") });
  assert.deepEqual(await (await call(env, "GET", "/v1/pair/host")).json(), { message: b64("给主持的 1") });
  assert.deepEqual(await (await call(env, "GET", "/v1/pair/join")).json(), { message: b64("给加入的") });
  assert.deepEqual(await (await call(env, "GET", "/v1/pair/host")).json(), { message: b64("给主持的 2") });
  assert.equal((await call(env, "GET", "/v1/pair/host")).status, 204);
  assert.equal((await call(env, "GET", "/v1/pair/nobody")).status, 404);
  assert.equal((await call(env, "POST", "/v1/pair/host", { token: "wrong", body: b64("x") })).status, 401);

  // 等着的时候来了留言
  const waiting = call(env, "GET", "/v1/pair/join?wait=20").then((r) => r.json());
  await new Promise((resolve) => setTimeout(resolve, 50));
  await call(env, "POST", "/v1/pair/join", { body: b64("来了") });
  assert.deepEqual(await waiting, { message: b64("来了") });

  // 清空
  await call(env, "POST", "/v1/pair/host", { body: b64("没人要的") });
  assert.deepEqual(await (await call(env, "DELETE", "/v1/pair")).json(), {});
  assert.equal((await call(env, "GET", "/v1/pair/host")).status, 204);
});

test("配对留言过期", async (t) => {
  const env = makeEnv();
  await call(env, "POST", "/v1/pair/host", { body: b64("过期的") });
  t.mock.timers.enable({ apis: ["Date"], now: Date.now() + limits.PAIR_TTL_MS + 1000 });
  assert.equal((await call(env, "GET", "/v1/pair/host")).status, 204);
});
