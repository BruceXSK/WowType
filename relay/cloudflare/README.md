# WowType 跨设备复制粘贴的中转服务

两台电脑不在同一个局域网（公司和家里）时，剪贴板内容经这个服务转一下。它跑在 Cloudflare Workers 上，
**你自己部署、自己用**，免费套餐够用。WowType 没有自己的服务器，也不知道你部署在哪。

它只看得到密文：内容在设备上用配对时换来的组密钥加了密，这里没有密钥。它知道的只有：有几台设备在收发、
什么时候发的、每条多大。每条最多留 10 分钟、每组设备最多留 20 条，过了就删。

同一个局域网里的设备用不着它，WowType 会直接传。两条路可以同时开。

## 部署

要一个 Cloudflare 账号（免费的就行）和 Node.js。

```bash
git clone https://github.com/Gloridust/WowType.git
cd WowType/relay/cloudflare
npx wrangler deploy
```

第一次会让你登录 Cloudflare。部署完会打印服务的地址，类似 `https://wowtype-sync-relay.<你的子域>.workers.dev`。

再设一个访问口令（没有口令的人用不了你的服务）。口令自己编，够长就行，只能用英文字母、数字和符号：

```bash
openssl rand -base64 24        # 生成一串
npx wrangler secret put TOKEN  # 把它贴进去
```

没设口令之前服务什么都不给（返回 503）。

部署完可以让它自己走一遍接口，确认能用（用的是随机的信道，不碰你设备上的内容）：

```bash
node test/live.mjs https://wowtype-sync-relay.<你的子域>.workers.dev <访问口令>
```

## 在 WowType 里填

每台设备上：WowType 设置 →「输入」→「跨设备复制粘贴」→「更多选项」，填上面的地址和访问口令，点「测试」。
然后照常配对（一台显示配对码，另一台输入）。两台设备都填了同一个中转服务时，不在同一个局域网也能配对。

## 用量

免费套餐每天 10 万次请求。一台设备开着长轮询时大约每 25 秒一次请求（一天 3000 多次）；最近 10 分钟没人用
输入法就改成一分钟问一次。

## 接口

都要带 `Authorization: Bearer <访问口令>`（`GET /` 除外）。信道名是 32 位十六进制，由组密钥派生。

| 请求 | 作用 |
|---|---|
| `GET /` | `{"service":"wowtype-sync-relay","version":1}`，看部署好了没有 |
| `GET /v1/ping` | `{"ok":true}`，验口令 |
| `POST /v1/c/<信道>` | 正文是 Base64 的信封，返回 `{"seq":N}` |
| `GET /v1/c/<信道>?after=N&wait=S` | N 以后的信封，没有就最多等 S 秒（上限 25）。不带 `after` 只回现在的 `seq` |
| `POST /v1/pair/<host\|join>`、`GET /v1/pair/<host\|join>?wait=S`、`DELETE /v1/pair` | 配对时两边互相留言（留 5 分钟） |

想换别的平台自己实现一份也行，接口就这些；客户端在 `sync/src/relay.rs`。

## 测试

```bash
node --test relay/cloudflare/test/relay.test.mjs
```

Worker 的代码直接在 node 里跑（假的 Durable Object 存储）。`cargo test -p wowtype-sync` 里中转服务那几条
也是拿它测的。

想在本机用 Cloudflare 的运行时（workerd，真的 Durable Object）跑：

```bash
cd relay/cloudflare
echo 'TOKEN=随便一串' > .dev.vars
npx wrangler dev --port 8791        # 另开一个终端
node test/live.mjs http://127.0.0.1:8791 随便一串
```

CI 里的「中转服务（workerd）」任务跑的就是这个。本机地址可以直接填进 WowType（只有本机地址允许用 `http://`），
拿 `wowtype-sync pair-host / pair-join / listen / send` 加 `--relay http://127.0.0.1:8791 --no-lan` 能走完整个流程。
