// 对着一个真的在跑的中转服务（本机的 `npx wrangler dev`，或者部署到 Cloudflare 上的）走一遍接口。
// 部署完想确认它能用：
//
//   node relay/cloudflare/test/live.mjs https://wowtype-sync-relay.<你的子域>.workers.dev <访问口令>
//
// 用的是随机的信道名，不会碰到你设备上的内容；配对留言那一步会清空信箱，别在正配对的时候跑。
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";

const [base, token] = process.argv.slice(2);
if (!base || !token) {
  console.error("用法：node live.mjs <地址> <访问口令>");
  process.exit(2);
}
const url = base.replace(/\/+$/, "");
const channel = "/v1/c/" + randomBytes(16).toString("hex");
const b64 = (text) => Buffer.from(text).toString("base64");

async function call(method, path, { auth = token, body } = {}) {
  const headers = auth === null ? {} : { Authorization: "Bearer " + auth };
  const response = await fetch(url + path, { method, headers, body });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {}
  return { status: response.status, json };
}

let step = 0;
async function check(name, run) {
  await run();
  console.log(`ok   ${++step}. ${name}`);
}

await check("部署好了：根路径认得出来", async () => {
  const { status, json } = await call("GET", "/", { auth: null });
  assert.equal(status, 200);
  assert.deepEqual(json, { service: "wowtype-sync-relay", version: 1 });
});

await check("访问口令管用：对的进得来，错的、没有的进不来", async () => {
  assert.deepEqual((await call("GET", "/v1/ping")).json, { ok: true });
  assert.equal((await call("GET", "/v1/ping", { auth: token + "x" })).status, 401);
  assert.equal((await call("GET", "/v1/ping", { auth: null })).status, 401);
  assert.equal((await call("POST", channel, { auth: "wrong", body: b64("x") })).status, 401);
});

await check("发、取，新设备不翻旧账", async () => {
  assert.deepEqual((await call("GET", channel)).json, { seq: 0, items: [] });
  assert.deepEqual((await call("POST", channel, { body: b64("一") })).json, { seq: 1 });
  assert.deepEqual((await call("POST", channel, { body: b64("二") })).json, { seq: 2 });
  assert.deepEqual((await call("GET", channel)).json, { seq: 2, items: [] });
  assert.deepEqual((await call("GET", channel + "?after=0")).json, { seq: 2, items: [b64("一"), b64("二")] });
  assert.deepEqual((await call("GET", channel + "?after=1")).json, { seq: 2, items: [b64("二")] });
  assert.equal((await call("POST", channel, { body: "不是 base64" })).status, 400);
});

await check("长轮询：别的设备一发，等着的马上拿到", async () => {
  const started = Date.now();
  const waiting = call("GET", channel + "?after=2&wait=20");
  await new Promise((resolve) => setTimeout(resolve, 500));
  await call("POST", channel, { body: b64("新的") });
  assert.deepEqual((await waiting).json, { seq: 3, items: [b64("新的")] });
  assert.ok(Date.now() - started < 10000, "有新的应该马上回");
});

await check("长轮询：没有新的等到时间再回", async () => {
  const started = Date.now();
  assert.deepEqual((await call("GET", channel + "?after=3&wait=2")).json, { seq: 3, items: [] });
  assert.ok(Date.now() - started >= 1500, "应该等满 2 秒左右");
});

await check("只留最近 20 条", async () => {
  for (let i = 0; i < 22; i++) await call("POST", channel, { body: b64("第" + i) });
  const { json } = await call("GET", channel + "?after=0");
  assert.equal(json.seq, 25);
  assert.equal(json.items.length, 20);
});

await check("配对留言：各取各的，取走就没了", async () => {
  await call("DELETE", "/v1/pair");
  assert.equal((await call("GET", "/v1/pair/host")).status, 204);
  await call("POST", "/v1/pair/host", { body: b64("给主持的") });
  await call("POST", "/v1/pair/join", { body: b64("给加入的") });
  assert.deepEqual((await call("GET", "/v1/pair/join")).json, { message: b64("给加入的") });
  assert.deepEqual((await call("GET", "/v1/pair/host")).json, { message: b64("给主持的") });
  assert.equal((await call("GET", "/v1/pair/host")).status, 204);
  const waiting = call("GET", "/v1/pair/join?wait=20");
  await new Promise((resolve) => setTimeout(resolve, 500));
  await call("POST", "/v1/pair/join", { body: b64("等到了") });
  assert.deepEqual((await waiting).json, { message: b64("等到了") });
  await call("DELETE", "/v1/pair");
});

console.log("中转服务能用");
