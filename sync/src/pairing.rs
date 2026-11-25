//! 配对：一台设备（主持的）显示 6 位配对码，另一台（加入的）输入，换来组密钥。
//!
//! 配对码只有 6 位数字，不能直接当密钥用，也不能让线路上的人拿去离线猜。所以走 SPAKE2：两边各拿配对码
//! 算一条消息、交换，得出同一把会话密钥；线路上的人（中转服务、同一个网里的别人）看到这两条消息也算不出
//! 配对码，想冒充只能在线一次猜一个。主持的一方猜错 [`MAX_ATTEMPTS`] 次、或者过了 [`CODE_TTL`]，
//! 这个配对码就作废。
//!
//! 流程（[`Channel`] 是局域网的 TCP 连接，或者中转服务上的信箱）：
//!
//! ```text
//! 加入的 → 主持的   HELLO    会话号 + SPAKE2 消息
//! 主持的 → 加入的   HELLO    会话号 + SPAKE2 消息
//! 加入的 → 主持的   CONFIRM  HMAC(会话密钥, 两条消息)        证明自己知道配对码
//! 主持的 → 加入的   BUNDLE   HMAC(…) + 加密的{组密钥, 主持的设备}   配对码不对回 REJECT
//! 加入的 → 主持的   ACK      加密的{加入的设备}
//! ```

use std::io;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::crypto::{
    b64_decode, b64_encode, derive, hex_decode, hex_encode, hmac, open_with, random, same,
    seal_with, KEY_LEN,
};
use crate::store::Device;

/// 配对码多久作废。
pub const CODE_TTL: Duration = Duration::from_secs(300);
/// 配对码输错几次作废（每次是对方在线猜一次）。
pub const MAX_ATTEMPTS: u32 = 5;
/// 等对方一条消息最多多久。
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

const HELLO_JOIN: u8 = 1;
const HELLO_HOST: u8 = 2;
const CONFIRM: u8 = 3;
const BUNDLE: u8 = 4;
const ACK: u8 = 5;
const REJECT: u8 = 6;

const SESSION_LEN: usize = 8;
const MAC_LEN: usize = 32;

const ID_HOST: &[u8] = b"wowtype-sync host";
const ID_JOIN: &[u8] = b"wowtype-sync join";

#[derive(Debug)]
pub enum PairError {
    /// 配对码不对。
    WrongCode,
    /// 对方没回话。
    Timeout,
    /// 对方说的不是这套话（版本不对，或者不是 WowType）。
    Protocol(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::WrongCode => write!(f, "配对码不对"),
            PairError::Timeout => write!(f, "对方没有回应"),
            PairError::Protocol(what) => write!(f, "配对出错：{what}"),
            PairError::Io(e) => write!(f, "连接出错：{e}"),
        }
    }
}

impl From<io::Error> for PairError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => PairError::Timeout,
            _ => PairError::Io(e),
        }
    }
}

/// 配对时两边传消息的线路。一条消息整条收发。
pub trait Channel {
    fn send(&mut self, message: &[u8]) -> io::Result<()>;
    /// 等下一条消息；到时间还没有返回 `TimedOut`。
    fn recv(&mut self, timeout: Duration) -> io::Result<Vec<u8>>;
}

/// 生成 6 位配对码。
pub fn new_code() -> String {
    loop {
        // 取 [0, 4_000_000) 再模一百万，每个数出现的机会一样
        let n = u32::from_be_bytes(random()) & 0x3f_ffff;
        if n < 4_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

/// 配对码只认 6 位数字（中间有空格、短横的去掉）。
pub fn normalize_code(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

fn frame(kind: u8, session: &[u8; SESSION_LEN], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + SESSION_LEN + body.len());
    out.push(kind);
    out.extend_from_slice(session);
    out.extend_from_slice(body);
    out
}

/// 拆出（种类, 会话号, 内容）。
fn parse(message: &[u8]) -> Option<(u8, [u8; SESSION_LEN], &[u8])> {
    if message.len() < 1 + SESSION_LEN {
        return None;
    }
    let mut session = [0u8; SESSION_LEN];
    session.copy_from_slice(&message[1..1 + SESSION_LEN]);
    Some((message[0], session, &message[1 + SESSION_LEN..]))
}

fn confirm_mac(key: &[u8], who: &[u8], session: &[u8], join: &[u8], host: &[u8]) -> [u8; 32] {
    hmac(key, &[b"wowtype-sync v1 confirm ", who, session, join, host])
}

fn device_json(device: &Device) -> Value {
    json!({ "id": hex_encode(&device.id), "name": device.name })
}

fn device_from(value: &Value) -> Option<Device> {
    Some(Device {
        id: hex_decode(value.get("id")?.as_str()?)?.try_into().ok()?,
        // 名字是对方给的：只留一行、别太长
        name: value
            .get("name")?
            .as_str()?
            .chars()
            .filter(|c| !c.is_control())
            .take(40)
            .collect(),
    })
}

/// 主持的一方收到一条 HELLO 以后把这一次配对走完。返回加入的设备。
///
/// `hello` 是已经收到的第一条消息（局域网上一条连接一次，中转信箱里要先挑出 HELLO）。
/// 配对码不对返回 [`PairError::WrongCode`]，调用方记一次数。
pub fn host_exchange(
    channel: &mut dyn Channel,
    hello: &[u8],
    code: &str,
    group_key: &[u8; KEY_LEN],
    me: &Device,
) -> Result<Device, PairError> {
    let (kind, session, join_msg) = parse(hello).ok_or(PairError::Protocol("消息太短"))?;
    if kind != HELLO_JOIN {
        return Err(PairError::Protocol("不是配对请求"));
    }
    let (state, host_msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_JOIN),
        &Identity::new(ID_HOST),
    );
    // 消息本身不合法（长度、点不在曲线上）也按配对码不对算：都是对方用掉一次机会
    let key = state.finish(join_msg).map_err(|_| PairError::WrongCode)?;
    channel.send(&frame(HELLO_HOST, &session, &host_msg))?;

    let confirm = recv_kind(channel, &session, CONFIRM)?;
    if !same(
        &confirm,
        &confirm_mac(&key, b"join", &session, join_msg, &host_msg),
    ) {
        let _ = channel.send(&frame(REJECT, &session, &[]));
        return Err(PairError::WrongCode);
    }
    let bundle = json!({
        "group_key": b64_encode(group_key),
        "device": device_json(me),
    });
    let mut body = confirm_mac(&key, b"host", &session, join_msg, &host_msg).to_vec();
    body.extend(seal_with(
        &derive(&key, b"wowtype-sync v1 bundle"),
        &session,
        bundle.to_string().as_bytes(),
    ));
    channel.send(&frame(BUNDLE, &session, &body))?;

    let ack = recv_kind(channel, &session, ACK)?;
    let ack = open_with(&derive(&key, b"wowtype-sync v1 ack"), &session, &ack)
        .ok_or(PairError::Protocol("确认消息解不开"))?;
    serde_json::from_slice::<Value>(&ack)
        .ok()
        .as_ref()
        .and_then(device_from)
        .ok_or(PairError::Protocol("确认消息读不懂"))
}

/// 加入的一方：拿配对码和主持的一方换组密钥。返回（组密钥, 主持的设备）。
pub fn join(
    channel: &mut dyn Channel,
    code: &str,
    me: &Device,
) -> Result<([u8; KEY_LEN], Device), PairError> {
    let session: [u8; SESSION_LEN] = random();
    let (state, join_msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_JOIN),
        &Identity::new(ID_HOST),
    );
    channel.send(&frame(HELLO_JOIN, &session, &join_msg))?;
    let host_msg = recv_kind(channel, &session, HELLO_HOST)?;
    let key = state.finish(&host_msg).map_err(|_| PairError::WrongCode)?;
    channel.send(&frame(
        CONFIRM,
        &session,
        &confirm_mac(&key, b"join", &session, &join_msg, &host_msg),
    ))?;

    let body = recv_kind(channel, &session, BUNDLE)?;
    if body.len() < MAC_LEN {
        return Err(PairError::Protocol("消息太短"));
    }
    let (mac, sealed) = body.split_at(MAC_LEN);
    // 主持的一方也要证明知道配对码：不然随便谁都能塞一把自己的组密钥过来
    if !same(
        mac,
        &confirm_mac(&key, b"host", &session, &join_msg, &host_msg),
    ) {
        return Err(PairError::WrongCode);
    }
    let bundle = open_with(&derive(&key, b"wowtype-sync v1 bundle"), &session, sealed)
        .ok_or(PairError::Protocol("组密钥解不开"))?;
    let bundle: Value =
        serde_json::from_slice(&bundle).map_err(|_| PairError::Protocol("组密钥读不懂"))?;
    let group_key = bundle
        .get("group_key")
        .and_then(Value::as_str)
        .and_then(b64_decode)
        .and_then(|k| <[u8; KEY_LEN]>::try_from(k).ok())
        .ok_or(PairError::Protocol("组密钥读不懂"))?;
    let host = bundle
        .get("device")
        .and_then(device_from)
        .ok_or(PairError::Protocol("对方的设备信息读不懂"))?;
    channel.send(&frame(
        ACK,
        &session,
        &seal_with(
            &derive(&key, b"wowtype-sync v1 ack"),
            &session,
            device_json(me).to_string().as_bytes(),
        ),
    ))?;
    Ok((group_key, host))
}

/// 等这一次会话里某一种消息（中转信箱里可能混着上一次没走完的，跳过）。
fn recv_kind(
    channel: &mut dyn Channel,
    session: &[u8; SESSION_LEN],
    wanted: u8,
) -> Result<Vec<u8>, PairError> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(PairError::Timeout)?;
        let message = channel.recv(left)?;
        let Some((kind, theirs, body)) = parse(&message) else {
            continue;
        };
        if &theirs != session {
            continue;
        }
        if kind == REJECT {
            return Err(PairError::WrongCode);
        }
        if kind == wanted {
            return Ok(body.to_vec());
        }
    }
}

/// 是不是一条配对请求（主持的一方从信箱里挑）。
pub fn is_hello(message: &[u8]) -> bool {
    parse(message).is_some_and(|(kind, _, _)| kind == HELLO_JOIN)
}

/// 主持的一方管着的配对码：过期、输错太多次就作废。
pub struct HostCode {
    pub code: String,
    deadline: Instant,
    attempts_left: u32,
}

impl HostCode {
    pub fn new() -> Self {
        Self::with_code(new_code())
    }

    pub fn with_code(code: String) -> Self {
        Self {
            code,
            deadline: Instant::now() + CODE_TTL,
            attempts_left: MAX_ATTEMPTS,
        }
    }

    pub fn usable(&self) -> bool {
        self.attempts_left > 0 && Instant::now() < self.deadline
    }

    pub fn time_left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// 走一次配对；配对码不对记一次数。
    pub fn serve(
        &mut self,
        channel: &mut dyn Channel,
        hello: &[u8],
        group_key: &[u8; KEY_LEN],
        me: &Device,
    ) -> Result<Device, PairError> {
        if !self.usable() {
            return Err(PairError::WrongCode);
        }
        let result = host_exchange(channel, hello, &self.code, group_key, me);
        if matches!(result, Err(PairError::WrongCode)) {
            self.attempts_left -= 1;
        }
        result
    }
}

impl Default for HostCode {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};

    /// 内存里的一对线路。`tap` 记下所有经过的消息（当线路上的旁观者）。
    pub struct Pipe {
        tx: Sender<Vec<u8>>,
        rx: Receiver<Vec<u8>>,
        pub tap: Option<Sender<Vec<u8>>>,
    }

    pub fn pipe() -> (Pipe, Pipe) {
        let (a_tx, b_rx) = channel();
        let (b_tx, a_rx) = channel();
        (
            Pipe {
                tx: a_tx,
                rx: a_rx,
                tap: None,
            },
            Pipe {
                tx: b_tx,
                rx: b_rx,
                tap: None,
            },
        )
    }

    impl Channel for Pipe {
        fn send(&mut self, message: &[u8]) -> io::Result<()> {
            if let Some(tap) = &self.tap {
                let _ = tap.send(message.to_vec());
            }
            self.tx
                .send(message.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "对方走了"))
        }
        fn recv(&mut self, timeout: Duration) -> io::Result<Vec<u8>> {
            self.rx.recv_timeout(timeout).map_err(|e| match e {
                RecvTimeoutError::Timeout => io::Error::new(io::ErrorKind::TimedOut, "超时"),
                RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "对方走了")
                }
            })
        }
    }

    fn device(id: u8, name: &str) -> Device {
        Device {
            id: [id; 8],
            name: name.to_string(),
        }
    }

    fn run(host_code: &str, join_code: &str) -> (Result<Device, PairError>, Result<([u8; 32], Device), PairError>, Vec<Vec<u8>>) {
        let (mut host_side, mut join_side) = pipe();
        let (tap_tx, tap_rx) = channel();
        host_side.tap = Some(tap_tx.clone());
        join_side.tap = Some(tap_tx);
        let host_code = host_code.to_string();
        let host = std::thread::spawn(move || {
            let hello = host_side.recv(Duration::from_secs(5)).unwrap();
            assert!(is_hello(&hello));
            host_exchange(
                &mut host_side,
                &hello,
                &host_code,
                &[42u8; 32],
                &device(1, "主持的"),
            )
        });
        let joined = join(&mut join_side, join_code, &device(2, "加入的\n第二行"));
        let hosted = host.join().unwrap();
        (hosted, joined, tap_rx.try_iter().collect())
    }

    #[test]
    fn right_code_transfers_the_group_key() {
        let (hosted, joined, seen) = run("123456", "123456");
        let (key, host) = joined.unwrap();
        assert_eq!(key, [42u8; 32]);
        assert_eq!(host, device(1, "主持的"));
        let joiner = hosted.unwrap();
        assert_eq!(joiner.id, [2u8; 8]);
        assert_eq!(joiner.name, "加入的第二行", "名字里的换行去掉");
        // 线路上的人看到的所有消息里没有组密钥，也没有配对码
        let wire: Vec<u8> = seen.concat();
        assert!(!wire.windows(32).any(|w| w == [42u8; 32]));
        assert!(!wire.windows(6).any(|w| w == b"123456"));
        assert!(!String::from_utf8_lossy(&wire).contains(&b64_encode(&[42u8; 32])));
    }

    #[test]
    fn wrong_code_gets_nothing() {
        let (hosted, joined, seen) = run("123456", "654321");
        assert!(matches!(hosted, Err(PairError::WrongCode)), "{hosted:?}");
        assert!(matches!(joined, Err(PairError::WrongCode)));
        // 配对码不对时主持的一方根本不发 BUNDLE
        assert!(seen.iter().all(|m| m[0] != BUNDLE));
    }

    #[test]
    fn code_is_used_up_after_too_many_wrong_guesses() {
        let mut host = HostCode::with_code("111111".to_string());
        let me = device(1, "主持的");
        for _ in 0..MAX_ATTEMPTS {
            assert!(host.usable());
            let (mut host_side, mut join_side) = pipe();
            let guess = std::thread::spawn(move || join(&mut join_side, "222222", &device(3, "猜的")));
            let hello = host_side.recv(Duration::from_secs(5)).unwrap();
            let result = host.serve(&mut host_side, &hello, &[1u8; 32], &me);
            assert!(matches!(result, Err(PairError::WrongCode)));
            assert!(matches!(guess.join().unwrap(), Err(PairError::WrongCode)));
        }
        assert!(!host.usable(), "猜错太多次，配对码作废");
        // 作废以后拿着对的配对码也不行
        let (mut host_side, mut join_side) = pipe();
        let late = std::thread::spawn(move || {
            join_side.send(&frame(HELLO_JOIN, &[0u8; 8], &[0u8; 33])).unwrap();
        });
        let hello = host_side.recv(Duration::from_secs(5)).unwrap();
        late.join().unwrap();
        assert!(matches!(
            host.serve(&mut host_side, &hello, &[1u8; 32], &me),
            Err(PairError::WrongCode)
        ));
    }

    #[test]
    fn fake_host_cannot_plant_its_own_key() {
        // 假的主持方不知道配对码：它回的 HELLO、BUNDLE 过不了加入方的检查
        let (mut fake, mut join_side) = pipe();
        let attacker = std::thread::spawn(move || {
            let hello = fake.recv(Duration::from_secs(5)).unwrap();
            let _ = host_exchange(&mut fake, &hello, "000000", &[66u8; 32], &device(9, "假的"));
        });
        let joined = join(&mut join_side, "123456", &device(2, "加入的"));
        assert!(matches!(joined, Err(PairError::WrongCode)), "{joined:?}");
        attacker.join().unwrap();
    }

    #[test]
    fn stale_and_garbage_messages_are_skipped() {
        let (mut host_side, mut join_side) = pipe();
        // 信箱里留着上一次的消息、乱七八糟的东西
        host_side.send(&frame(HELLO_HOST, &[9u8; 8], b"old")).unwrap();
        host_side.send(b"x").unwrap();
        let host = std::thread::spawn(move || {
            let hello = host_side.recv(Duration::from_secs(5)).unwrap();
            host_exchange(&mut host_side, &hello, "123456", &[5u8; 32], &device(1, "主持的"))
        });
        let (key, _) = join(&mut join_side, "123456", &device(2, "加入的")).unwrap();
        assert_eq!(key, [5u8; 32]);
        host.join().unwrap().unwrap();
    }

    #[test]
    fn codes() {
        for _ in 0..200 {
            let code = new_code();
            assert_eq!(normalize_code(&code).as_deref(), Some(code.as_str()));
        }
        assert_eq!(normalize_code(" 123 456 ").as_deref(), Some("123456"));
        assert_eq!(normalize_code("123-456").as_deref(), Some("123456"));
        assert_eq!(normalize_code("12345"), None);
        assert_eq!(normalize_code("1234567"), None);
        assert_eq!(normalize_code("12345a"), None);
        assert_eq!(normalize_code("１２３４５６"), None);
    }
}
