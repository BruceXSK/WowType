// 在 node 里跑 Worker：假的 Durable Object 存储 + 命名空间，再包一层本机 HTTP。
// `node test/harness.mjs [端口] [口令]` 起一个本机的中转服务（Rust 那边的测试也用它），打印 `listening <端口>`。
import http from "node:http";
import worker, { Relay } from "../src/index.js";

/// 内存里的 Durable Object 存储（只有 Worker 用到的那几个方法）
export class Storage {
  constructor() {
    this.map = new Map();
  }
  async get(key) {
    return structuredClone(this.map.get(key));
  }
  async put(key, value) {
    const entries = typeof key === "string" ? { [key]: value } : key;
    for (const [k, v] of Object.entries(entries)) this.map.set(k, structuredClone(v));
  }
  async delete(key) {
    for (const k of Array.isArray(key) ? key : [key]) this.map.delete(k);
  }
  async list({ prefix = "" } = {}) {
    const keys = [...this.map.keys()].filter((k) => k.startsWith(prefix)).sort();
    return new Map(keys.map((k) => [k, structuredClone(this.map.get(k))]));
  }
}

export function makeEnv(token = "secret") {
  const objects = new Map();
  const env = {
    TOKEN: token,
    RELAY: {
      idFromName: (name) => name,
      get(id) {
        if (!objects.has(id)) objects.set(id, new Relay({ storage: new Storage() }, env));
        const object = objects.get(id);
        return { fetch: (request) => object.fetch(request) };
      },
    },
    objects,
  };
  return env;
}

export function call(env, method, path, { token = "secret", body } = {}) {
  const headers = token === null ? {} : { Authorization: "Bearer " + token };
  return worker.fetch(new Request("http://relay.test" + path, { method, headers, body }), env);
}

export function serve(env, port = 0) {
  const server = http.createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = ["GET", "HEAD"].includes(req.method) ? undefined : Buffer.concat(chunks);
    const response = await worker.fetch(
      new Request("http://127.0.0.1" + req.url, { method: req.method, headers: req.headers, body }),
      env,
    );
    res.writeHead(response.status, Object.fromEntries(response.headers));
    res.end(Buffer.from(await response.arrayBuffer()));
  });
  return new Promise((resolve) => server.listen(port, "127.0.0.1", () => resolve(server)));
}

if (import.meta.url === new URL(process.argv[1], "file:").href) {
  const server = await serve(makeEnv(process.argv[3] || "secret"), Number(process.argv[2] || 0));
  console.log("listening " + server.address().port);
}
