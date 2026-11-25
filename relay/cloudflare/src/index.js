// WowType 跨设备剪贴板的中转服务（Cloudflare Worker + Durable Object）。
//
// 你自己部署、自己用：设备之间不在同一个局域网时，剪贴板内容经它转一下。它只看得到密文——内容在
// 设备上用配对时换来的组密钥加了密，这里没有密钥，也不知道内容是什么。
//
// 接口（都要带 `Authorization: Bearer <TOKEN>`，TOKEN 是部署时设的访问口令）：
//
//   GET    /                      → {"service":"wowtype-sync-relay","version":1}（不用口令，看部署好了没有）
//   GET    /v1/ping               → {"ok":true}（设置界面「测试连接」，顺便验口令）
//   POST   /v1/c/<信道>           正文是 Base64 的信封 → {"seq":N}
//   GET    /v1/c/<信道>?after=N&wait=S
//                                 → {"seq":M,"items":["<Base64>",…]}：N 以后的信封；没有就最多等 S 秒。
//                                   不带 after 只回现在的 seq（新设备从这里开始，不翻旧账）
//   POST   /v1/pair/<host|join>   配对时两边互相留言，正文是 Base64 → {}
//   GET    /v1/pair/<host|join>?wait=S → {"message":"<Base64>"}，没有回 204
//   DELETE /v1/pair               清空配对留言 → {}
//
// 信道名是 32 位十六进制，由组密钥派生；每个信道一个 Durable Object。信封最多留 KEEP 条、
// TTL_MS 毫秒，过了就删。

const VERSION = 1;
/// 每个信道最多留多少条
const KEEP = 20;
/// 一条最多留多久（10 分钟）
const TTL_MS = 10 * 60 * 1000;
/// 配对留言最多留多久（5 分钟，和配对码一样）
const PAIR_TTL_MS = 5 * 60 * 1000;
/// 一条最大多少字节（Base64 以后；文字上限 16 KB，留足余量）
const MAX_BODY = 48 * 1024;
/// 长轮询最多等多久（秒）
const MAX_WAIT = 25;

function json(value, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" },
  });
}

/// 不随内容提前返回的比较
function sameText(a, b) {
  const x = new TextEncoder().encode(a);
  const y = new TextEncoder().encode(b);
  let diff = x.length ^ y.length;
  for (let i = 0; i < x.length; i++) diff |= x[i] ^ y[i % Math.max(y.length, 1)];
  return diff === 0;
}

function authorized(request, token) {
  const header = request.headers.get("Authorization") || "";
  return header.startsWith("Bearer ") && sameText(header.slice(7).trim(), token);
}

function waitSeconds(url) {
  const wait = Number(url.searchParams.get("wait") || 0);
  return Number.isFinite(wait) ? Math.min(Math.max(wait, 0), MAX_WAIT) : 0;
}

/// 读正文：只认不太长的 Base64
async function readBody(request) {
  const text = (await request.text()).trim();
  if (text.length === 0 || text.length > MAX_BODY || !/^[A-Za-z0-9+/]+={0,2}$/.test(text)) return null;
  return text;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/") return json({ service: "wowtype-sync-relay", version: VERSION });
    if (!env.TOKEN) return json({ error: "还没有设置访问口令（TOKEN）" }, 503);
    if (!authorized(request, String(env.TOKEN))) return json({ error: "访问口令不对" }, 401);
    if (url.pathname === "/v1/ping") return json({ ok: true });
    const channel = url.pathname.match(/^\/v1\/c\/([0-9a-f]{32})$/);
    if (channel) return env.RELAY.get(env.RELAY.idFromName("c:" + channel[1])).fetch(request);
    if (/^\/v1\/pair(\/(host|join))?$/.test(url.pathname)) {
      return env.RELAY.get(env.RELAY.idFromName("pair")).fetch(request);
    }
    return json({ error: "没有这个地址" }, 404);
  },
};

export class Relay {
  constructor(state) {
    this.storage = state.storage;
    /// 正在等新消息的请求：有新的就叫醒
    this.waiters = new Set();
  }

  wake() {
    for (const resolve of this.waiters) resolve();
    this.waiters.clear();
  }

  /// 等到有新消息或者到时间
  sleep(seconds) {
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        this.waiters.delete(done);
        resolve();
      };
      const timer = setTimeout(done, seconds * 1000);
      this.waiters.add(done);
    });
  }

  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname.startsWith("/v1/pair")) return this.pair(request, url);
    if (request.method === "POST") return this.push(request);
    if (request.method === "GET") return this.poll(url);
    return json({ error: "不支持这个方法" }, 405);
  }

  // ---- 剪贴板信封

  /// 留着的信封，按 seq 从小到大；过期的顺手删掉
  async items(now = Date.now()) {
    const kept = [];
    const stale = [];
    for (const [key, item] of await this.storage.list({ prefix: "i:" })) {
      if (now - item.at > TTL_MS) stale.push(key);
      else kept.push(item);
    }
    kept.sort((a, b) => a.seq - b.seq);
    while (kept.length > KEEP) stale.push(itemKey(kept.shift().seq));
    if (stale.length) await this.storage.delete(stale);
    return kept;
  }

  async push(request) {
    const data = await readBody(request);
    if (!data) return json({ error: "正文要是 Base64，且不超过 " + MAX_BODY + " 字节" }, 400);
    const seq = ((await this.storage.get("seq")) || 0) + 1;
    await this.storage.put({ seq, [itemKey(seq)]: { seq, at: Date.now(), data } });
    await this.items();
    this.wake();
    return json({ seq });
  }

  async poll(url) {
    const seq = (await this.storage.get("seq")) || 0;
    if (!url.searchParams.has("after")) return json({ seq, items: [] });
    let after = Number(url.searchParams.get("after"));
    if (!Number.isInteger(after) || after < 0) return json({ error: "after 要是整数" }, 400);
    // 存储被清过（seq 从头来了）：客户端手里的 seq 比这里大，从头给
    if (after > seq) after = 0;
    let items = (await this.items()).filter((item) => item.seq > after);
    const wait = waitSeconds(url);
    if (items.length === 0 && wait > 0) {
      await this.sleep(wait);
      items = (await this.items()).filter((item) => item.seq > after);
    }
    return json({ seq: (await this.storage.get("seq")) || 0, items: items.map((item) => item.data) });
  }

  // ---- 配对留言

  async pair(request, url) {
    if (request.method === "DELETE" && url.pathname === "/v1/pair") {
      const keys = [...(await this.storage.list({ prefix: "m:" })).keys()];
      if (keys.length) await this.storage.delete(keys);
      return json({});
    }
    const inbox = url.pathname.slice("/v1/pair/".length);
    if (inbox !== "host" && inbox !== "join") return json({ error: "没有这个地址" }, 404);
    if (request.method === "POST") {
      const data = await readBody(request);
      if (!data) return json({ error: "正文要是 Base64" }, 400);
      const n = ((await this.storage.get("mseq")) || 0) + 1;
      await this.storage.put({ mseq: n, [mailKey(inbox, n)]: { at: Date.now(), data } });
      this.wake();
      return json({});
    }
    if (request.method === "GET") {
      let message = await this.take(inbox);
      const wait = waitSeconds(url);
      if (message === null && wait > 0) {
        await this.sleep(wait);
        message = await this.take(inbox);
      }
      return message === null ? new Response(null, { status: 204 }) : json({ message });
    }
    return json({ error: "不支持这个方法" }, 405);
  }

  /// 取走信箱里最早的一条（过期的删掉）
  async take(inbox) {
    const now = Date.now();
    for (const [key, mail] of await this.storage.list({ prefix: "m:" + inbox + ":" })) {
      await this.storage.delete(key);
      if (now - mail.at <= PAIR_TTL_MS) return mail.data;
    }
    return null;
  }
}

/// 键按字典序就是按 seq 排（补零到 12 位）
function itemKey(seq) {
  return "i:" + String(seq).padStart(12, "0");
}

function mailKey(inbox, n) {
  return "m:" + inbox + ":" + String(n).padStart(12, "0");
}

export const limits = { KEEP, TTL_MS, PAIR_TTL_MS, MAX_BODY, MAX_WAIT };
