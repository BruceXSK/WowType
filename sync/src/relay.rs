//! 中转服务的客户端：不在同一个局域网的设备经用户自己部署的中转服务（`relay/cloudflare`）传信封。
//!
//! 和互译一样走系统的 `curl`（`curl -K -`，配置从标准输入给，访问口令不出现在命令行里）：不用把
//! TLS 库编进输入法，代理、证书也都跟着系统走。中转服务只看得到密文和信道名。
//!
//! 取新消息是长轮询：一次请求最多挂 [`POLL_WAIT`] 秒，有新的马上回。

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::crypto::{b64_decode, b64_encode};
use crate::pairing::Channel;

/// 长轮询一次最多等多少秒（中转服务那边的上限也是 25）。
pub const POLL_WAIT: u64 = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// 地址不能用（要 https://；只有本机地址可以用 http://）。
    BadUrl,
    /// 访问口令里有不能用的字符（只能是英文字母、数字和符号，不能有空格）。
    BadToken,
    /// 访问口令不对。
    Unauthorized,
    /// 中转服务回了别的状态码。
    Http(u16),
    /// 连不上、超时、没装 curl。
    Network(String),
    /// 回的东西读不懂（地址填成了别的网站）。
    BadResponse,
    /// 被 [`Canceller::cancel`] 打断了。
    Cancelled,
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::BadUrl => write!(f, "中转服务的地址要以 https:// 开头"),
            RelayError::BadToken => {
                write!(f, "访问口令只能用英文字母、数字和符号，中间不能有空格")
            }
            RelayError::Unauthorized => write!(f, "中转服务的访问口令不对"),
            RelayError::Http(503) => write!(f, "中转服务还没设置访问口令（TOKEN）"),
            RelayError::Http(code) => write!(f, "中转服务返回 {code}"),
            RelayError::Network(why) => write!(f, "连不上中转服务：{why}"),
            RelayError::BadResponse => write!(f, "这个地址不是 WowType 的中转服务"),
            RelayError::Cancelled => write!(f, "已取消"),
        }
    }
}

/// 打断正在挂着的请求（停用同步、换设置、退出时）。
#[derive(Default)]
pub struct Canceller {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl Canceller {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay {
    base: String,
    token: String,
    loopback: bool,
}

impl Relay {
    /// `url` 是部署好的地址（`https://xxx.workers.dev`），`token` 是部署时设的访问口令。
    pub fn new(url: &str, token: &str) -> Result<Self, RelayError> {
        let base = url.trim().trim_end_matches('/').to_string();
        let host = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
            .ok_or(RelayError::BadUrl)?
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("");
        let name = host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                name
            } else {
                host
            }
        });
        let loopback = matches!(name, "localhost" | "127.0.0.1" | "[::1]");
        // 明文 http 会把访问口令露在线路上，只让本机地址用（测试、自己套了隧道）
        if name.is_empty() || (base.starts_with("http://") && !loopback) {
            return Err(RelayError::BadUrl);
        }
        // 要写进 curl 的配置：不许有控制字符、引号、空白
        let clean = |s: &str| {
            !s.chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '\\')
        };
        if !clean(&base) {
            return Err(RelayError::BadUrl);
        }
        // 口令放在 HTTP 头里，只能是可见的 ASCII
        let token = token.trim();
        if !clean(token) || !token.is_ascii() {
            return Err(RelayError::BadToken);
        }
        Ok(Self {
            base,
            token: token.to_string(),
            loopback,
        })
    }

    fn config(&self, method: &str, path: &str, body: Option<&str>, max_time: u64) -> String {
        let mut config = format!(
            "url = \"{}{}\"\nrequest = \"{}\"\nheader = \"Authorization: Bearer {}\"\n\
             max-time = {}\nconnect-timeout = 10\nwrite-out = \"\\n%{{http_code}}\"\n",
            self.base, path, method, self.token, max_time
        );
        if self.loopback {
            config.push_str("noproxy = \"*\"\n");
        }
        if let Some(body) = body {
            // 正文是 Base64，不用转义
            config.push_str("header = \"Content-Type: text/plain\"\n");
            config.push_str(&format!("data-binary = \"{body}\"\n"));
        }
        config
    }

    /// 发一个请求，返回（状态码, 正文）。
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        max_time: u64,
        canceller: Option<&Canceller>,
    ) -> Result<(u16, String), RelayError> {
        if canceller.is_some_and(Canceller::is_cancelled) {
            return Err(RelayError::Cancelled);
        }
        let mut command = Command::new("curl");
        command
            .args(["-sS", "-K", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| RelayError::Network(format!("起不来 curl（{e}）")))?;
        let mut stdin = child.stdin.take().expect("stdin 是管道");
        let mut stdout = child.stdout.take().expect("stdout 是管道");
        let mut stderr = child.stderr.take().expect("stderr 是管道");
        let config = self.config(method, path, body, max_time);
        // 交给取消的人，好让它能杀掉挂着的 curl
        let mut own = Some(child);
        if let Some(canceller) = canceller {
            let mut slot = canceller.child.lock().unwrap();
            *slot = own.take();
            // 刚才检查完到现在之间被取消了：cancel 那时还没有子进程可杀
            if canceller.is_cancelled() {
                if let Some(child) = slot.as_mut() {
                    let _ = child.kill();
                }
            }
        }
        let written = stdin.write_all(config.as_bytes());
        drop(stdin);
        let mut output = String::new();
        let read = stdout.read_to_string(&mut output);
        let mut errors = String::new();
        let _ = stderr.read_to_string(&mut errors);
        let mut child = match canceller {
            Some(canceller) => canceller.child.lock().unwrap().take(),
            None => own.take(),
        }
        .expect("子进程还在");
        let status = child.wait();
        if canceller.is_some_and(Canceller::is_cancelled) {
            return Err(RelayError::Cancelled);
        }
        if written.is_err() || read.is_err() {
            return Err(RelayError::Network("和 curl 说不上话".to_string()));
        }
        if !status.map(|s| s.success()).unwrap_or(false) {
            return Err(RelayError::Network(curl_reason(&errors)));
        }
        let (text, code) = output.rsplit_once('\n').ok_or(RelayError::BadResponse)?;
        let code: u16 = code.trim().parse().map_err(|_| RelayError::BadResponse)?;
        match code {
            401 | 403 => Err(RelayError::Unauthorized),
            200..=299 => Ok((code, text.to_string())),
            _ => Err(RelayError::Http(code)),
        }
    }

    fn json(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        max_time: u64,
        canceller: Option<&Canceller>,
    ) -> Result<Value, RelayError> {
        let (_, text) = self.request(method, path, body, max_time, canceller)?;
        serde_json::from_str(&text).map_err(|_| RelayError::BadResponse)
    }

    /// 「测试连接」：地址对不对、口令对不对。
    pub fn ping(&self) -> Result<(), RelayError> {
        let about = self.json("GET", "/", None, 15, None)?;
        if about.get("service").and_then(Value::as_str) != Some("wowtype-sync-relay") {
            return Err(RelayError::BadResponse);
        }
        let pong = self.json("GET", "/v1/ping", None, 15, None)?;
        (pong.get("ok") == Some(&Value::Bool(true)))
            .then_some(())
            .ok_or(RelayError::BadResponse)
    }

    /// 发一个信封。
    pub fn push(&self, channel: &str, envelope: &[u8]) -> Result<u64, RelayError> {
        let reply = self.json(
            "POST",
            &format!("/v1/c/{channel}"),
            Some(&b64_encode(envelope)),
            20,
            None,
        )?;
        reply
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or(RelayError::BadResponse)
    }

    /// 取 `after` 以后的信封，没有就最多等 `wait` 秒。`after` 是 None 时只问现在到哪了（新设备、
    /// 刚打开同步时从这里开始，不翻旧账）。返回（新的位置, 信封）。
    pub fn poll(
        &self,
        channel: &str,
        after: Option<u64>,
        wait: u64,
        canceller: Option<&Canceller>,
    ) -> Result<(u64, Vec<Vec<u8>>), RelayError> {
        let path = match after {
            Some(after) => format!("/v1/c/{channel}?after={after}&wait={wait}"),
            None => format!("/v1/c/{channel}"),
        };
        let reply = self.json("GET", &path, None, wait + 15, canceller)?;
        let seq = reply
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or(RelayError::BadResponse)?;
        let items = reply
            .get("items")
            .and_then(Value::as_array)
            .ok_or(RelayError::BadResponse)?
            .iter()
            .filter_map(|item| b64_decode(item.as_str()?))
            .collect();
        Ok((seq, items))
    }

    /// 清空配对留言（主持的一方开始配对前）。
    pub fn clear_mail(&self) -> Result<(), RelayError> {
        self.json("DELETE", "/v1/pair", None, 15, None).map(|_| ())
    }

    fn mail_send(&self, inbox: &str, message: &[u8]) -> Result<(), RelayError> {
        self.json(
            "POST",
            &format!("/v1/pair/{inbox}"),
            Some(&b64_encode(message)),
            15,
            None,
        )
        .map(|_| ())
    }

    fn mail_recv(
        &self,
        inbox: &str,
        wait: u64,
        canceller: Option<&Canceller>,
    ) -> Result<Option<Vec<u8>>, RelayError> {
        let (code, text) = self.request(
            "GET",
            &format!("/v1/pair/{inbox}?wait={wait}"),
            None,
            wait + 15,
            canceller,
        )?;
        if code == 204 {
            return Ok(None);
        }
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| b64_decode(v.get("message")?.as_str()?))
            .map(Some)
            .ok_or(RelayError::BadResponse)
    }
}

/// curl 的报错里挑一句给人看的。
fn curl_reason(stderr: &str) -> String {
    let line = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let line = line.strip_prefix("curl: ").unwrap_or(line);
    if line.is_empty() {
        "网络不通".to_string()
    } else {
        line.chars().take(120).collect()
    }
}

/// 配对时经中转服务的信箱传消息：主持的收 `host` 信箱、往 `join` 信箱发，加入的反过来。
pub struct MailChannel<'a> {
    relay: &'a Relay,
    mine: &'static str,
    theirs: &'static str,
    canceller: Option<&'a Canceller>,
}

impl<'a> MailChannel<'a> {
    pub fn host(relay: &'a Relay, canceller: Option<&'a Canceller>) -> Self {
        Self {
            relay,
            mine: "host",
            theirs: "join",
            canceller,
        }
    }

    pub fn join(relay: &'a Relay, canceller: Option<&'a Canceller>) -> Self {
        Self {
            relay,
            mine: "join",
            theirs: "host",
            canceller,
        }
    }
}

fn io_error(e: RelayError) -> std::io::Error {
    let kind = match e {
        RelayError::Cancelled => std::io::ErrorKind::Interrupted,
        RelayError::Unauthorized => std::io::ErrorKind::PermissionDenied,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, e.to_string())
}

impl Channel for MailChannel<'_> {
    fn send(&mut self, message: &[u8]) -> std::io::Result<()> {
        self.relay.mail_send(self.theirs, message).map_err(io_error)
    }

    fn recv(&mut self, timeout: Duration) -> std::io::Result<Vec<u8>> {
        let wait = timeout.as_secs().clamp(1, POLL_WAIT);
        match self
            .relay
            .mail_recv(self.mine, wait, self.canceller)
            .map_err(io_error)?
        {
            Some(message) => Ok(message),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "还没有消息",
            )),
        }
    }
}

/// 测试用：在 node 里起真的 Worker 代码（`relay/cloudflare/test/harness.mjs`）。没装 node 返回 None。
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::io::{BufRead, BufReader};

    pub struct TestRelay {
        child: Child,
        pub url: String,
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn harness() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../relay/cloudflare/test/harness.mjs")
    }

    pub fn start(token: &str) -> Option<TestRelay> {
        let mut child = Command::new("node")
            .arg(harness())
            .args(["0", token])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        let Some(port) = line.trim().strip_prefix("listening ") else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        Some(TestRelay {
            child,
            url: format!("http://127.0.0.1:{port}"),
        })
    }

    /// 没装 node、没装 curl 的机器上跳过中转服务的测试。
    pub fn available() -> bool {
        let ok = |program: &str| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        // 发行版打包时只带了一部分源码目录：没有 Worker 的代码也跳过
        let all = ok("node") && ok("curl") && harness().is_file();
        if !all {
            eprintln!("skip：中转服务的测试要 node、curl 和 relay/cloudflare 的代码");
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    const CHANNEL: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn urls() {
        assert!(Relay::new("https://relay.example.workers.dev", "t").is_ok());
        assert!(Relay::new(" https://relay.example.workers.dev/ ", "t").is_ok());
        assert!(Relay::new("http://127.0.0.1:8787", "t").is_ok());
        assert!(Relay::new("http://localhost:8787/", "t").is_ok());
        for bad in [
            "",
            "relay.example.com",
            "http://relay.example.com",
            "ftp://relay.example.com",
            "file:///etc/passwd",
            "https://",
            "https://a b.example.com",
            "https://example.com/\"\nurl = \"file:///etc/passwd",
            "http://127.0.0.1.evil.example.com",
            "http://localhost.evil.example.com:80",
        ] {
            assert_eq!(Relay::new(bad, "t").err(), Some(RelayError::BadUrl), "{bad}");
        }
        for bad in ["a\"\nurl = \"http://x", "有中文", "a b", "back\\slash"] {
            assert_eq!(
                Relay::new("https://example.com", bad).err(),
                Some(RelayError::BadToken),
                "{bad}"
            );
        }
        // 口令不出现在命令行里：只在给 curl 的配置（标准输入）里
        let relay = Relay::new("https://example.com", "tok").unwrap();
        let config = relay.config("GET", "/v1/ping", None, 5);
        assert!(config.contains("Authorization: Bearer tok"));
        assert!(!config.contains("noproxy"));
    }

    #[test]
    fn push_and_poll() {
        if !available() {
            return;
        }
        let server = start("token-abc").unwrap();
        let relay = Relay::new(&server.url, "token-abc").unwrap();
        relay.ping().unwrap();
        assert_eq!(
            Relay::new(&server.url, "wrong").unwrap().ping(),
            Err(RelayError::Unauthorized)
        );
        assert_eq!(relay.poll(CHANNEL, None, 0, None).unwrap(), (0, vec![]));
        assert_eq!(relay.push(CHANNEL, b"first").unwrap(), 1);
        assert_eq!(relay.push(CHANNEL, &[0, 255, 10, 13]).unwrap(), 2);
        // 新设备从现在开始，不翻旧账
        assert_eq!(relay.poll(CHANNEL, None, 0, None).unwrap(), (2, vec![]));
        assert_eq!(
            relay.poll(CHANNEL, Some(0), 0, None).unwrap(),
            (2, vec![b"first".to_vec(), vec![0, 255, 10, 13]])
        );
        assert_eq!(relay.poll(CHANNEL, Some(2), 0, None).unwrap(), (2, vec![]));

        // 长轮询：别的设备一发，这边马上拿到
        let pusher = relay.clone();
        let later = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            pusher.push(CHANNEL, b"pushed").unwrap();
        });
        let started = Instant::now();
        let (seq, items) = relay.poll(CHANNEL, Some(2), 20, None).unwrap();
        assert_eq!((seq, items), (3, vec![b"pushed".to_vec()]));
        assert!(started.elapsed() < Duration::from_secs(10));
        later.join().unwrap();
    }

    #[test]
    fn long_poll_can_be_cancelled() {
        if !available() {
            return;
        }
        let server = start("t").unwrap();
        let relay = Relay::new(&server.url, "t").unwrap();
        let canceller = Arc::new(Canceller::default());
        let stopper = Arc::clone(&canceller);
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            stopper.cancel();
        });
        let started = Instant::now();
        let result = relay.poll(CHANNEL, Some(0), 20, Some(&canceller));
        assert_eq!(result, Err(RelayError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5), "取消以后不该等满 20 秒");
        cancel.join().unwrap();
        // 取消过的不再发请求
        assert_eq!(
            relay.poll(CHANNEL, None, 0, Some(&canceller)),
            Err(RelayError::Cancelled)
        );
    }

    #[test]
    fn unreachable_and_wrong_site() {
        if !available() {
            return;
        }
        // 没人听的端口
        let relay = Relay::new("http://127.0.0.1:9", "t").unwrap();
        assert!(matches!(relay.ping(), Err(RelayError::Network(_))));
    }

    #[test]
    fn pairing_through_the_mailbox() {
        if !available() {
            return;
        }
        use crate::pairing::{is_hello, join, HostCode};
        use crate::store::Device;
        let server = start("t").unwrap();
        let relay = Relay::new(&server.url, "t").unwrap();
        relay.clear_mail().unwrap();
        let host_relay = relay.clone();
        let host = std::thread::spawn(move || {
            let mut code = HostCode::with_code("246810".to_string());
            let mut channel = MailChannel::host(&host_relay, None);
            let hello = loop {
                let message = channel.recv(Duration::from_secs(10)).unwrap();
                if is_hello(&message) {
                    break message;
                }
            };
            code.serve(
                &mut channel,
                &hello,
                &[77u8; 32],
                &Device {
                    id: [1u8; 8],
                    name: "主持的".to_string(),
                },
            )
        });
        let (key, host_device) = join(
            &mut MailChannel::join(&relay, None),
            "246810",
            &Device {
                id: [2u8; 8],
                name: "加入的".to_string(),
            },
        )
        .unwrap();
        assert_eq!(key, [77u8; 32]);
        assert_eq!(host_device.name, "主持的");
        assert_eq!(host.join().unwrap().unwrap().name, "加入的");
    }
}
