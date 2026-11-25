# 跨设备复制粘贴

一台电脑上复制的文字，在另一台装了 WowType 的电脑上直接粘贴。类似微信输入法的「跨设备粘贴」，
不同的是没有账号、不经过 WowType 的服务器，内容端到端加密。

Linux（fcitx5）、macOS、Windows 三端都有，可以互相配对（见[各端的情况](#各端的情况)）。

## 怎么用

1. 两台设备都打开 WowType 设置 →「输入」→「跨设备复制粘贴」（Windows 在「跨设备」页，改完要点「保存」）。
2. 在一台上点「显示配对码」，在另一台上输入这 6 位数字。配对码 5 分钟内有效，输错 5 次作废。
3. 配好以后，一台上复制的文字，在另一台上按 `Ctrl+V`（macOS 是 `Command+V`），或者打 `v` 再按空格，在「剪贴板」里选。

第三台设备找已经配好对的任何一台再配一次就行。

只传文字，一条最多 16 KB。图片和文件不传，见[为什么不传文件](#为什么不传文件)。

## 两条路

| | 局域网直连 | 自建中转服务 |
|---|---|---|
| 什么时候用得上 | 两台设备在同一个局域网 | 不在同一个局域网（公司和家里） |
| 要准备什么 | 防火墙放行 39217 的 UDP 和 TCP | 自己部署一个 Cloudflare Worker，见 [relay/cloudflare](../relay/cloudflare/README.md) |
| 经过谁 | 不经过任何服务器 | 你自己的 Worker，它只看得到密文 |
| 设置项 | `ClipboardSyncLan`（默认开） | `ClipboardSyncRelayUrl` 和访问口令（默认不用） |

两条路可以同时开：同一条内容从两边各来一次，只收一次。

## 安全

- **配对**：配对码只有 6 位，不直接当密钥用。两边用它走 SPAKE2 换出一把会话密钥，线路上的人（中转服务、
  同一个网里的别人）看到全部消息也算不出配对码，想冒充只能在线一次猜一个，猜错 5 次这个配对码就作废。
  会话密钥只用来把 32 字节的**组密钥**传过去，之后这组设备都拿这把组密钥。
- **内容**：每条用组密钥派生的密钥加密（XChaCha20-Poly1305），发送设备和时间一起验。不是这一组的拆不开、
  改不了；同一条重放的、时间和本机差一小时以上的丢掉。
- **局域网发现**：广播的小消息带签名（组密钥派生的 HMAC），不是同一组的认不出彼此，也没法把信引到别处。
- **存哪**：组密钥在 `sync.json`，中转服务的访问口令在 `sync-relay.key`（Linux 在 `~/.config/wowtype/`，
  macOS 在 `~/Library/Application Support/WowType/`，Windows 在 `%LOCALAPPDATA%\WowType\`），Linux、macOS 上是 0600。
  不进 `wowtype.conf`、不进日志。日志里只记收到多少字节、哪台设备发的。
- **中转服务知道什么**：有几台设备、什么时候发、每条多大。不知道内容，也不知道组密钥。

**要注意的**：

- **Linux 上密码管理器复制的密码也会被传过去。** fcitx5 的 clipboard 插件不告诉输入法哪条是密码（和「记录剪贴板历史」
  一样的原因），所以这个功能默认关。光标在密码框里时不读剪贴板。macOS、Windows 上密码管理器给剪贴板标了
  「别拿走」的不传（macOS 是 nspasteboard.org 的 ConcealedType 等，Windows 是 `ExcludeClipboardContentFromMonitorProcessing`、
  `CanIncludeInClipboardHistory`、`CanUploadToCloudClipboard`）；没标的照传。
- **踢掉一台设备**：组密钥是共用的，「取消配对」只让这台设备自己忘掉密钥。设备丢了的话，剩下的每台都取消配对，
  再重新配一次（会换一把新的组密钥）。
- 用的加密库（RustCrypto 的 `spake2`、`chacha20poly1305`）没有全部经过独立审计；协议是这个项目自己设计的。

## 设置项

| 键 | 默认 | 作用 |
|---|---|---|
| `ClipboardSync` | `False` | 总开关 |
| `ClipboardSyncLan` | `True` | 局域网直连 |
| `ClipboardSyncRelayUrl` | 空 | 中转服务的地址（`https://…`），空着不用 |
| `ClipboardSyncDeviceName` | 空 | 这台设备的名字，显示在别的设备上；空着用主机名 |
| `ClipboardSyncPaste` | `True` | 收到的也放进系统剪贴板（`Ctrl+V` 能贴）；关掉只出现在 `v` + 空格的「剪贴板」里。Windows 没有这一项，总是放进系统剪贴板 |

## 各端的情况

| | 状态 |
|---|---|
| Linux（fcitx5） | 能用。读剪贴板靠 fcitx5 自带的 clipboard 插件，每秒看一次；写系统剪贴板靠 `wl-copy`（Wayland）或 `xclip` / `xsel`（X11），都没装时只能在 `v` + 空格里选 |
| GNOME 的 Wayland 会话 | **没验证**。GNOME 不给后台程序读剪贴板的接口，fcitx5 的 clipboard 插件在这上面可能读不到别的应用里复制的内容，那样这台设备就**发不出去**（收不受影响）。以后可以让 WowType 的 GNOME 扩展来读 |
| macOS | 输入法进程每秒看一次系统剪贴板（变了才读），别的设备复制的放进系统剪贴板。配对在设置窗口「输入」页。**没在真机上试过**（设置窗口的代码只过了 CI 的编译，核心那层的封装有测试） |
| Windows | 输入法是加载进每个应用进程的 DLL，所以收发在一个后台进程里（`wowtype-settings.exe --sync-agent`）：在设置窗口「跨设备」页打开并保存时起，以后登录时自己起，关掉就退。别的设备复制的放进系统剪贴板。第一次用局域网直连时系统防火墙会问要不要放行。**没在真机上试过**（只过了 CI 的编译） |

## 为什么不传文件

评估过，这一版不做，原因：

1. **输入法贴不了文件。** 文字可以由输入法自己上屏；文件只能写进系统剪贴板，每个平台的格式都不一样，
   GNOME 的 Wayland 会话里后台程序根本写不了。
2. **Linux 上读不到。** fcitx5 的 clipboard 插件只给文字。
3. **免费的中转服务装不下。** 现在一条 16 KB，Durable Object 就够了；文件要另接对象存储（R2）、分片、续传、
   限额，自建的门槛高出一截。
4. **别人也是分开做的。** 微信输入法的「跨设备粘贴」只有文字和图片，文件走的是另一个功能「隔空传送」
   （设备之间直连，连不上才走服务器）；KDE Connect 的剪贴板同步也只有文字，文件走「分享」。

图片是可以接着做的一步（macOS、Windows 读写剪贴板里的图片都方便；Linux 要先解决读的问题）。
局域网里传文件更适合做成单独的「发送文件」，不走剪贴板。

## 排查

`wowtype-sync` 是配对用的命令行（设置界面调的就是它），也能拿来查问题：

```bash
wowtype-sync status --state ~/.config/wowtype/sync.json   # 配没配对、和谁配的
wowtype-sync listen --state ~/.config/wowtype/sync.json   # 不经输入法，直接听：收到什么打什么
wowtype-sync ping --relay https://… --relay-key-file ~/.config/wowtype/sync-relay.key
```

- **局域网里收不到**：多半是防火墙。`sudo ufw allow 39217`（UFW），或
  `sudo firewall-cmd --add-port=39217/tcp --add-port=39217/udp --permanent && sudo firewall-cmd --reload`（firewalld）。
  路由器开了「AP 隔离」、连的是访客网络时设备之间互相看不到，只能用中转服务。
- **配对时找不到另一台**：两台要在同一个局域网，另一台要正显示着配对码；或者两台都填好同一个中转服务。
- **两台设备的时间差一小时以上**会收不到（防重放），把系统时间对准。

## 实现

| 在哪 | 是什么 |
|---|---|
| `sync/` | 配对（`pairing.rs`）、加密（`crypto.rs`）、局域网（`lan.rs`）、中转服务的客户端（`relay.rs`，和互译一样走系统的 curl）、同步服务（`service.rs`）、命令行（`cli.rs`） |
| `relay/cloudflare/` | 中转服务（Cloudflare Worker + Durable Object） |
| `core/src/sync_ffi.rs` | C 接口 `wowtype_sync_*`（ABI 1.19）：同步服务，和配对（`wowtype_sync_pair_*`），见 [ABI.md](ABI.md) |
| `fcitx5-addon` | 每秒采一次剪贴板交给核心，取回别的设备复制的 |
| `settings/wowtype-config` | Linux 设置「输入」页的「跨设备复制粘贴」一组（配对调 `wowtype-sync` 命令行） |
| `macos/Sources/Shared/ClipboardSync.swift`、`App/AppState.swift`、`App/SettingsSync.swift` | macOS：核心接口的封装、输入法进程里的收发、设置窗口的配对 |
| `windows/src/settings/settings_app.cpp` | Windows：设置窗口的「跨设备」页，和后台进程 `--sync-agent` |

测试：`cargo test -p wowtype-sync`（两台「设备」在本机走局域网、走 node 里跑的真 Worker 代码）、
`scripts/addon-test.sh`（输入法和命令行起的另外两台设备互相收发）、`scripts/test-settings-gui.py`（设置界面真配对）、
`macos/test.sh`（Swift 封装真配对）、`relay/cloudflare/test/live.mjs`（对着 Cloudflare 的本地运行时或者真的部署走一遍接口）。
