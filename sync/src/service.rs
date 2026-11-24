//! 同步服务：输入法里跑的那一份。外壳告诉它本机剪贴板现在是什么，它负责加密、发出去、收进来。
//!
//! 线程都是自己的，外壳调的函数不阻塞：
//!
//! - 管理线程：照设置起停下面这些线程；盯着配对文件，设置界面配好对（或者取消配对）马上跟着变。
//! - 局域网：一个线程广播 / 收广播，一个线程收信。
//! - 中转服务：一个线程长轮询。最近没人用输入法（[`ACTIVE_WINDOW`]）就改成隔一阵问一次，
//!   不一直挂着连接。
//! - 发送线程：把要发的信封送给局域网里的设备和中转服务。

use std::collections::{HashSet, VecDeque};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::crypto::{envelope_sender, DeviceId, GroupKeys, NONCE_LEN};
use crate::lan::{self, Peers};
use crate::relay::{Canceller, Relay, RelayError, POLL_WAIT};
use crate::store::{read_secret_line, State};

/// 一条最多多少字节（和工具栏的剪贴板历史一样）。
pub const MAX_TEXT: usize = 16 * 1024;
/// 收到的最多攒多少条等外壳来取。
const MAX_PENDING: usize = 20;
/// 信封上的时间和本机差多少以内才收（防重放；两台设备的钟差太多会收不到）。
const MAX_SKEW_MS: u64 = 60 * 60 * 1000;
/// 多久没人用输入法就不再长轮询。
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// 闲着的时候隔多久问一次中转服务。
const IDLE_POLL: Duration = Duration::from_secs(60);
/// 刚收到的内容外壳写进系统剪贴板以后会被当成「本机刚复制的」报回来：这么久以内认得出来、不往回发。
const ECHO_WINDOW: Duration = Duration::from_secs(600);
const TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    /// 局域网直连。
    pub lan: bool,
    /// 中转服务的地址；空着不用。
    pub relay_url: String,
    /// 放中转服务访问口令的文件（只有一行）。
    pub relay_key_file: Option<PathBuf>,
    /// 配对文件（[`State`]）。
    pub state_file: PathBuf,
    /// 这台设备叫什么（显示在别的设备上）。
    pub device_name: String,
    /// 局域网端口，一般是 [`lan::PORT`]；测试里各用各的。
    pub lan_port: u16,
    /// 隔多久广播一次。
    pub beacon_interval: Duration,
    /// 除了广播地址，还往这些地址发（测试里两个服务在同一台机器上，广播到不了回环地址）。
    pub extra_targets: Vec<SocketAddr>,
}

impl Config {
    pub fn new(state_file: PathBuf) -> Self {
        Self {
            enabled: false,
            lan: true,
            relay_url: String::new(),
            relay_key_file: None,
            state_file,
            device_name: String::new(),
            lan_port: lan::PORT,
            beacon_interval: Duration::from_secs(5),
            extra_targets: Vec::new(),
        }
    }
}

/// 别的设备复制过来的一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub text: String,
    /// 哪台设备复制的（它自己起的名字）。
    pub from: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RelayStatus {
    /// 没填地址。
    #[default]
    Off,
    /// 填了地址，但没有访问口令。
    NoToken,
    Connecting,
    Ok,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    pub enabled: bool,
    pub paired: bool,
    /// 配过对的别的设备的名字。
    pub devices: Vec<String>,
    pub lan: bool,
    /// 收信的 TCP 端口（0 = 没在听）。
    pub lan_port: u16,
    /// 局域网里现在看得到几台同组设备。
    pub lan_peers: usize,
    pub lan_error: Option<String>,
    pub relay: RelayStatus,
    pub sent: u64,
    pub received: u64,
    /// 配对文件读不了之类的问题。
    pub error: Option<String>,
}

impl Status {
    /// 给设置界面、诊断信息看的 JSON。
    pub fn to_json(&self) -> String {
        let (relay, relay_error) = match &self.relay {
            RelayStatus::Off => ("off", None),
            RelayStatus::NoToken => ("no-token", None),
            RelayStatus::Connecting => ("connecting", None),
            RelayStatus::Ok => ("ok", None),
            RelayStatus::Error(why) => ("error", Some(why.clone())),
        };
        json!({
            "enabled": self.enabled,
            "paired": self.paired,
            "devices": self.devices,
            "lan": self.lan,
            "lan_port": self.lan_port,
            "lan_peers": self.lan_peers,
            "lan_error": self.lan_error,
            "relay": relay,
            "relay_error": relay_error,
            "sent": self.sent,
            "received": self.received,
            "error": self.error,
        })
        .to_string()
    }
}

#[derive(Default)]
struct Inner {
    /// 正在跑的那一组线程的发送口；没在跑是 None。
    outgoing: Option<Sender<String>>,
    /// 外壳上一次报的本机剪贴板内容。重新配置以后清空：第一次报的只当基准，不发。
    last_observed: Option<String>,
    pending: VecDeque<Received>,
    /// 最近收到的内容（外壳把它写进系统剪贴板再报回来时不往回发）。
    recent_remote: VecDeque<(String, Instant)>,
    /// 最近见过的信封编号（同一封从局域网和中转各来一次，只收一次）。
    seen: HashSet<[u8; NONCE_LEN]>,
    seen_order: VecDeque<[u8; NONCE_LEN]>,
    last_touch: Option<Instant>,
    status: Status,
}

struct Shared {
    inner: Mutex<Inner>,
    peers: Mutex<Peers>,
}

enum Command {
    Configure(Box<Config>),
    Shutdown,
}

pub struct Service {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    manager: Mutex<Option<JoinHandle<()>>>,
}

impl Default for Service {
    fn default() -> Self {
        Self::new()
    }
}

impl Service {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner::default()),
            peers: Mutex::new(Peers::default()),
        });
        let (commands, inbox) = channel();
        let manager = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("wowtype-sync".to_string())
                .spawn(move || manage(shared, inbox))
                .ok()
        };
        Self {
            shared,
            commands,
            manager: Mutex::new(manager),
        }
    }

    /// 照设置起停。不阻塞；设置没变时什么都不做。
    pub fn configure(&self, config: Config) {
        let _ = self.commands.send(Command::Configure(Box::new(config)));
    }

    /// 外壳报告：本机剪贴板现在是这段文字。和上次报的不一样、又不是别的设备刚传来的，就发给别的设备。
    /// 重新配置以后第一次报的只当基准（打开同步时剪贴板里原有的东西不发）。返回发了没有。
    pub fn observe(&self, text: &str) -> bool {
        let mut inner = self.shared.inner.lock().unwrap();
        let first = inner.last_observed.is_none();
        if inner.last_observed.as_deref() == Some(text) {
            return false;
        }
        inner.last_observed = Some(text.to_string());
        if first || text.trim().is_empty() || text.len() > MAX_TEXT {
            return false;
        }
        inner
            .recent_remote
            .retain(|(_, at)| at.elapsed() < ECHO_WINDOW);
        if inner.recent_remote.iter().any(|(remote, _)| remote == text) {
            return false;
        }
        match &inner.outgoing {
            Some(outgoing) => outgoing.send(text.to_string()).is_ok(),
            None => false,
        }
    }

    /// 有人在用输入法：中转服务那边保持长轮询。
    pub fn touch(&self) {
        self.shared.inner.lock().unwrap().last_touch = Some(Instant::now());
    }

    /// 取一条别的设备复制过来的；没有返回 None。
    pub fn poll(&self) -> Option<Received> {
        self.shared.inner.lock().unwrap().pending.pop_front()
    }

    pub fn status(&self) -> Status {
        let mut status = self.shared.inner.lock().unwrap().status.clone();
        status.lan_peers = self.shared.peers.lock().unwrap().count();
        status
    }

    /// 停掉所有线程并等它们结束（退出输入法、测试收尾）。
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(manager) = self.manager.lock().unwrap().take() {
            let _ = manager.join();
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // 不等线程：外壳卸载插件时不该卡住；线程看到通道断了自己会停
        let _ = self.commands.send(Command::Shutdown);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 正在跑的一组线程。
struct Run {
    stop: Arc<AtomicBool>,
    canceller: Arc<Canceller>,
    threads: Vec<JoinHandle<()>>,
}

impl Run {
    fn stop(self, shared: &Shared) {
        self.stop.store(true, Ordering::SeqCst);
        self.canceller.cancel();
        shared.inner.lock().unwrap().outgoing = None;
        for thread in self.threads {
            let _ = thread.join();
        }
        shared.peers.lock().unwrap().clear();
    }
}

/// 配对文件的「版本」：改没改过。
fn stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn manage(shared: Arc<Shared>, inbox: Receiver<Command>) {
    let mut config: Option<Config> = None;
    let mut run: Option<Run> = None;
    let mut seen_stamp = None;
    loop {
        let command = match inbox.recv_timeout(Duration::from_secs(2)) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Command::Shutdown),
        };
        let restart = match command {
            Some(Command::Shutdown) => {
                if let Some(run) = run.take() {
                    run.stop(&shared);
                }
                return;
            }
            Some(Command::Configure(new)) => {
                let changed = config.as_ref() != Some(&*new);
                config = Some(*new);
                changed
            }
            // 设置界面配好对、取消配对：配对文件变了就照新的来
            None => config
                .as_ref()
                .is_some_and(|c| c.enabled && stamp(&c.state_file) != seen_stamp),
        };
        if !restart {
            continue;
        }
        if let Some(run) = run.take() {
            run.stop(&shared);
        }
        let config = config.as_ref().expect("刚设置过");
        seen_stamp = stamp(&config.state_file);
        run = start(&shared, config);
    }
}

/// 照设置起线程。没打开、没配对时只更新状态。
fn start(shared: &Arc<Shared>, config: &Config) -> Option<Run> {
    let mut status = Status {
        enabled: config.enabled,
        lan: config.lan,
        ..Status::default()
    };
    let state = match State::load(&config.state_file) {
        Ok(state) => state,
        Err(e) => {
            status.error = Some(e.to_string());
            State::fresh()
        }
    };
    status.paired = state.paired();
    status.devices = state.devices.iter().map(|d| d.name.clone()).collect();
    let relay = if config.relay_url.trim().is_empty() {
        None
    } else {
        match config.relay_key_file.as_deref().and_then(read_secret_line) {
            None => {
                status.relay = RelayStatus::NoToken;
                None
            }
            Some(token) => match Relay::new(&config.relay_url, &token) {
                Ok(relay) => {
                    status.relay = RelayStatus::Connecting;
                    Some(relay)
                }
                Err(e) => {
                    status.relay = RelayStatus::Error(e.to_string());
                    None
                }
            },
        }
    };
    let group_key = state.group_key.filter(|_| config.enabled);
    let mut inner = shared.inner.lock().unwrap();
    // 重新来过：剪贴板里现有的东西不算新复制的；没取走的不留给下一组设备
    inner.last_observed = None;
    inner.pending.clear();
    inner.status = status;
    let Some(group_key) = group_key else {
        return None;
    };
    let keys = GroupKeys::derive(&group_key);
    let stop = Arc::new(AtomicBool::new(false));
    let canceller = Arc::new(Canceller::default());
    let (outgoing, to_send) = channel::<String>();
    inner.outgoing = Some(outgoing);
    drop(inner);

    let context = Arc::new(Context {
        shared: Arc::clone(shared),
        keys,
        device: state.device_id,
        name: config.device_name.clone(),
        stop: Arc::clone(&stop),
    });
    let mut threads = Vec::new();
    let mut spawn = |name: &str, body: Box<dyn FnOnce() + Send>| {
        if let Ok(handle) = thread::Builder::new().name(name.to_string()).spawn(body) {
            threads.push(handle);
        }
    };
    if config.lan {
        match lan::bind_tcp(config.lan_port) {
            Ok(listener) => {
                let port = listener.local_addr().map_or(0, |a| a.port());
                context.shared.inner.lock().unwrap().status.lan_port = port;
                let ctx = Arc::clone(&context);
                spawn(
                    "wowtype-sync-lan-in",
                    Box::new(move || accept_loop(&ctx, listener)),
                );
                let ctx = Arc::clone(&context);
                let interval = config.beacon_interval;
                let udp_port = config.lan_port;
                let mut targets = lan::broadcast_targets(config.lan_port);
                targets.extend(config.extra_targets.iter().copied());
                spawn(
                    "wowtype-sync-lan-find",
                    Box::new(move || beacon_loop(&ctx, udp_port, port, interval, targets)),
                );
            }
            Err(e) => context.lan_error(format!("局域网收信的端口打不开：{e}")),
        }
    }
    if let Some(relay) = relay.clone() {
        let ctx = Arc::clone(&context);
        let canceller = Arc::clone(&canceller);
        spawn(
            "wowtype-sync-relay",
            Box::new(move || relay_loop(&ctx, relay, &canceller)),
        );
    }
    {
        let ctx = Arc::clone(&context);
        spawn(
            "wowtype-sync-send",
            Box::new(move || send_loop(&ctx, to_send, relay)),
        );
    }
    Some(Run {
        stop,
        canceller,
        threads,
    })
}

/// 一组线程共用的东西。
struct Context {
    shared: Arc<Shared>,
    keys: GroupKeys,
    device: DeviceId,
    name: String,
    stop: Arc<AtomicBool>,
}

impl Context {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn lan_error(&self, why: String) {
        self.shared.inner.lock().unwrap().status.lan_error = Some(why);
    }

    fn relay_status(&self, status: RelayStatus) {
        if !self.stopped() {
            self.shared.inner.lock().unwrap().status.relay = status;
        }
    }

    /// 收到一个信封（从局域网或者中转服务）。
    fn accept(&self, envelope: &[u8]) {
        if envelope_sender(envelope) == Some(self.device) {
            return;
        }
        let Some(opened) = self.keys.open(envelope) else {
            return;
        };
        if opened.ts_ms.abs_diff(now_ms()) > MAX_SKEW_MS {
            return;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(&opened.plaintext) else {
            return;
        };
        let Some(text) = payload.get("text").and_then(Value::as_str) else {
            return;
        };
        if text.trim().is_empty() || text.len() > MAX_TEXT {
            return;
        }
        let from: String = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(40)
            .collect();
        let mut inner = self.shared.inner.lock().unwrap();
        if self.stopped() || !inner.seen.insert(opened.id) {
            return;
        }
        inner.seen_order.push_back(opened.id);
        if inner.seen_order.len() > 512 {
            if let Some(old) = inner.seen_order.pop_front() {
                inner.seen.remove(&old);
            }
        }
        inner
            .recent_remote
            .retain(|(_, at)| at.elapsed() < ECHO_WINDOW);
        inner
            .recent_remote
            .push_back((text.to_string(), Instant::now()));
        while inner.recent_remote.len() > MAX_PENDING {
            inner.recent_remote.pop_front();
        }
        inner.pending.push_back(Received {
            text: text.to_string(),
            from,
        });
        while inner.pending.len() > MAX_PENDING {
            inner.pending.pop_front();
        }
        inner.status.received += 1;
    }
}

fn send_loop(ctx: &Context, to_send: Receiver<String>, relay: Option<Relay>) {
    loop {
        let text = match to_send.recv_timeout(TICK) {
            Ok(text) => text,
            Err(RecvTimeoutError::Timeout) if !ctx.stopped() => continue,
            Err(_) => return,
        };
        if ctx.stopped() {
            return;
        }
        let payload = json!({ "text": text, "name": ctx.name }).to_string();
        let envelope = ctx.keys.seal(&ctx.device, now_ms(), payload.as_bytes());
        // 先走局域网（马上到），再走中转服务（要起 curl，慢一点）
        let peers = ctx.shared.peers.lock().unwrap().alive();
        for addr in peers {
            let _ = lan::send_frame(addr, &envelope);
        }
        if let Some(relay) = &relay {
            match relay.push(&ctx.keys.channel, &envelope) {
                Ok(_) => ctx.relay_status(RelayStatus::Ok),
                Err(e) => ctx.relay_status(RelayStatus::Error(e.to_string())),
            }
        }
        ctx.shared.inner.lock().unwrap().status.sent += 1;
    }
}

fn beacon_loop(
    ctx: &Context,
    udp_port: u16,
    tcp_port: u16,
    interval: Duration,
    targets: Vec<SocketAddr>,
) {
    // 发现用的端口是固定的，偶尔会被别的程序临时占着（浏览器给 DNS 查询随机挑端口时可能正好挑中）：
    // 绑不上先报出来，隔一会儿再试，绑上了把报错清掉
    let socket = loop {
        match lan::bind_udp(udp_port) {
            Ok(socket) => {
                ctx.shared.inner.lock().unwrap().status.lan_error = None;
                break socket;
            }
            Err(e) => {
                ctx.lan_error(format!("局域网发现用的端口（{udp_port}）打不开：{e}"));
                nap(ctx, interval, false);
                if ctx.stopped() {
                    return;
                }
            }
        }
    };
    let _ = socket.set_read_timeout(Some(TICK));
    let beacon = lan::beacon(&ctx.keys, &ctx.device, tcp_port);
    let mut last_sent: Option<Instant> = None;
    let mut buffer = [0u8; 256];
    while !ctx.stopped() {
        if last_sent.map_or(true, |at| at.elapsed() >= interval) {
            for target in &targets {
                let _ = socket.send_to(&beacon, target);
            }
            last_sent = Some(Instant::now());
        }
        let Ok((len, from)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        if let Some((device, port)) = lan::parse_beacon(&ctx.keys, &buffer[..len]) {
            if device != ctx.device {
                let known = {
                    let mut peers = ctx.shared.peers.lock().unwrap();
                    let known = peers.alive().contains(&SocketAddr::new(from.ip(), port));
                    peers.saw(device, SocketAddr::new(from.ip(), port));
                    known
                };
                // 新来的设备：马上回一条，不用等它听到我们下一次广播
                if !known {
                    let _ = socket.send_to(&beacon, from);
                }
            }
        }
    }
}

fn accept_loop(ctx: &Arc<Context>, listener: TcpListener) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    // 同时处理的连接数：谁都能连上来，别让它把线程开爆
    let busy = Arc::new(AtomicUsize::new(0));
    while !ctx.stopped() {
        match listener.accept() {
            Ok((stream, _)) => {
                if busy.load(Ordering::SeqCst) >= 8 {
                    continue;
                }
                busy.fetch_add(1, Ordering::SeqCst);
                let ctx = Arc::clone(ctx);
                let done = Arc::clone(&busy);
                let spawned = thread::Builder::new()
                    .name("wowtype-sync-lan-conn".to_string())
                    .spawn(move || {
                        if let Ok(frame) = lan::receive_frame(stream) {
                            ctx.accept(&frame);
                        }
                        done.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    // 线程没起来：闭包没跑，计数自己减回去
                    busy.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn relay_loop(ctx: &Context, relay: Relay, canceller: &Canceller) {
    let mut cursor: Option<u64> = None;
    let mut backoff = Duration::from_secs(2);
    while !ctx.stopped() {
        let active = ctx
            .shared
            .inner
            .lock()
            .unwrap()
            .last_touch
            .is_some_and(|at| at.elapsed() < ACTIVE_WINDOW);
        let wait = if active && cursor.is_some() {
            POLL_WAIT
        } else {
            0
        };
        match relay.poll(&ctx.keys.channel, cursor, wait, Some(canceller)) {
            Ok((seq, items)) => {
                backoff = Duration::from_secs(2);
                ctx.relay_status(RelayStatus::Ok);
                for item in &items {
                    ctx.accept(item);
                }
                let first = cursor.is_none();
                cursor = Some(seq);
                if first || active {
                    continue;
                }
                // 没人在用：隔一阵再问；有人回来了（touch）马上接着问
                nap(ctx, IDLE_POLL, true);
            }
            Err(RelayError::Cancelled) => return,
            Err(e) => {
                ctx.relay_status(RelayStatus::Error(e.to_string()));
                nap(ctx, backoff, false);
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// 睡一会儿；停了马上醒，`wake_on_touch` 时有人用输入法了也醒。
fn nap(ctx: &Context, long: Duration, wake_on_touch: bool) {
    let started = Instant::now();
    while started.elapsed() < long && !ctx.stopped() {
        thread::sleep(TICK);
        if wake_on_touch
            && ctx
                .shared
                .inner
                .lock()
                .unwrap()
                .last_touch
                .is_some_and(|at| at > started)
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::test_support;
    use std::net::{Ipv4Addr, UdpSocket};

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "wowtype-sync-service-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn free_port() -> u16 {
        crate::lan::test_port()
    }

    /// 一台配好对的「设备」：状态文件里是同一把组密钥。
    fn device(dir: &Dir, name: &str, key: [u8; 32]) -> Config {
        let state_file = dir.0.join(format!("{name}.json"));
        let mut state = State::fresh();
        state.group_key = Some(key);
        state.save(&state_file).unwrap();
        let mut config = Config::new(state_file);
        config.enabled = true;
        config.device_name = name.to_string();
        config.lan_port = free_port();
        config.beacon_interval = Duration::from_millis(100);
        config
    }

    fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(found) = probe() {
                return Some(found);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn local(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    /// 起两台互相找得到的设备。
    fn lan_pair(dir: &Dir, key_a: [u8; 32], key_b: [u8; 32]) -> (Service, Service) {
        let mut a = device(dir, "书房", key_a);
        let mut b = device(dir, "客厅", key_b);
        a.extra_targets = vec![local(b.lan_port)];
        b.extra_targets = vec![local(a.lan_port)];
        let (sa, sb) = (Service::new(), Service::new());
        sa.configure(a);
        sb.configure(b);
        (sa, sb)
    }

    #[test]
    fn copies_travel_over_lan() {
        let dir = Dir::new("lan");
        let (a, b) = lan_pair(&dir, [1u8; 32], [1u8; 32]);
        assert!(
            wait_for(|| (a.status().lan_peers == 1 && b.status().lan_peers == 1).then_some(()))
                .is_some(),
            "两台设备没找到彼此：{:?} {:?}",
            a.status(),
            b.status()
        );
        // 打开同步时剪贴板里原有的东西不发
        assert!(!a.observe("打开之前就在剪贴板里的"));
        assert!(!b.observe("另一台原有的"));
        assert!(a.observe("第一句 hello"));
        let got = wait_for(|| b.poll()).expect("另一台没收到");
        assert_eq!(
            got,
            Received {
                text: "第一句 hello".to_string(),
                from: "书房".to_string()
            }
        );
        assert_eq!(b.poll(), None, "只收一次");
        // 外壳把收到的写进系统剪贴板、又当成本机的报回来：不往回发
        assert!(!b.observe("第一句 hello"));
        // 同样的内容连着报不重复发；真的又复制了别的才发
        assert!(!a.observe("第一句 hello"));
        assert!(b.observe("回一句"));
        assert_eq!(wait_for(|| a.poll()).unwrap().text, "回一句");
        assert_eq!(a.poll(), None);
        // 空白、太长的不发
        assert!(!a.observe("   \n"));
        assert!(!a.observe(&"长".repeat(MAX_TEXT)));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(b.poll(), None);
        let (sa, sb) = (a.status(), b.status());
        assert!(sa.enabled && sa.paired && sa.lan && sa.lan_port != 0);
        assert_eq!((sa.sent, sa.received, sb.sent, sb.received), (1, 1, 1, 1));
        assert!(sa.to_json().contains("\"lan_peers\":1"));
        a.shutdown();
        b.shutdown();
    }

    #[test]
    fn empty_clipboard_at_start_does_not_swallow_the_first_copy() {
        let dir = Dir::new("empty");
        let (a, b) = lan_pair(&dir, [9u8; 32], [9u8; 32]);
        wait_for(|| (a.status().lan_peers == 1 && b.status().lan_peers == 1).then_some(()))
            .expect("两台设备没找到彼此");
        // 刚开机还没复制过东西：外壳报的是空串，基准就是「空」
        assert!(!a.observe(""));
        assert!(!a.observe(""));
        assert!(a.observe("开机后第一次复制的"), "第一次复制的不该被当成基准");
        assert_eq!(wait_for(|| b.poll()).unwrap().text, "开机后第一次复制的");
        // 清空剪贴板不发；清空以后再复制同一段是新的一次复制
        assert!(!a.observe(""));
        assert!(a.observe("开机后第一次复制的"));
        assert_eq!(wait_for(|| b.poll()).unwrap().text, "开机后第一次复制的");
        a.shutdown();
        b.shutdown();
    }

    #[test]
    fn discovery_port_busy_at_start_is_retried() {
        let dir = Dir::new("busy");
        let mut a = device(&dir, "甲", [7u8; 32]);
        let mut b = device(&dir, "乙", [7u8; 32]);
        a.extra_targets = vec![local(b.lan_port)];
        b.extra_targets = vec![local(a.lan_port)];
        // 别的程序正占着乙的发现端口（没开端口复用，我们绑不上）
        let squatter = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, b.lan_port)).unwrap();
        let (sa, sb) = (Service::new(), Service::new());
        sa.configure(a);
        sb.configure(b);
        let error = wait_for(|| sb.status().lan_error).expect("端口被占着应该报出来");
        assert!(error.contains("打不开"), "{error}");
        assert_eq!(sb.status().lan_peers, 0);
        // 它走了以后自己恢复
        drop(squatter);
        wait_for(|| {
            (sb.status().lan_error.is_none()
                && sa.status().lan_peers == 1
                && sb.status().lan_peers == 1)
                .then_some(())
        })
        .expect("端口空出来以后没恢复");
        sa.shutdown();
        sb.shutdown();
    }

    #[test]
    fn other_groups_see_nothing() {
        let dir = Dir::new("groups");
        let (a, b) = lan_pair(&dir, [1u8; 32], [2u8; 32]);
        thread::sleep(Duration::from_millis(600));
        assert_eq!((a.status().lan_peers, b.status().lan_peers), (0, 0), "不同组的认不出彼此");
        assert!(!a.observe("基准"));
        assert!(a.observe("别的组收不到的"));
        // 就算知道地址、直接把信封塞过去，也拆不开
        let stranger = GroupKeys::derive(&[1u8; 32]);
        let envelope = stranger.seal(&[9u8; 8], now_ms(), r#"{"text":"塞进来的","name":"x"}"#.as_bytes());
        lan::send_frame(local(b.status().lan_port), &envelope).unwrap();
        lan::send_frame(local(b.status().lan_port), b"garbage").unwrap();
        thread::sleep(Duration::from_millis(400));
        assert_eq!(b.poll(), None);
        assert_eq!(b.status().received, 0);
        a.shutdown();
        b.shutdown();
    }

    #[test]
    fn replays_and_stale_envelopes_are_dropped() {
        let dir = Dir::new("replay");
        let config = device(&dir, "本机", [5u8; 32]);
        let service = Service::new();
        service.configure(config);
        let port = wait_for(|| Some(service.status().lan_port).filter(|p| *p != 0)).unwrap();
        let keys = GroupKeys::derive(&[5u8; 32]);
        let payload = r#"{"text":"抄下来重放的","name":"别的设备"}"#.as_bytes();
        let envelope = keys.seal(&[9u8; 8], now_ms(), payload);
        lan::send_frame(local(port), &envelope).unwrap();
        assert_eq!(wait_for(|| service.poll()).unwrap().text, "抄下来重放的");
        // 同一个信封再送一次（局域网里有人抄下来重放）：不收
        lan::send_frame(local(port), &envelope).unwrap();
        // 时间差太多的（很久以前抄下来的）：不收
        let old = keys.seal(&[9u8; 8], now_ms() - MAX_SKEW_MS - 60_000, payload);
        lan::send_frame(local(port), &old).unwrap();
        // 内容不是文字的：不收
        let odd = keys.seal(&[9u8; 8], now_ms(), br#"{"text":42}"#);
        lan::send_frame(local(port), &odd).unwrap();
        thread::sleep(Duration::from_millis(400));
        assert_eq!(service.poll(), None);
        assert_eq!(service.status().received, 1);
        service.shutdown();
    }

    #[test]
    fn disabled_or_unpaired_does_nothing() {
        let dir = Dir::new("off");
        let service = Service::new();
        // 没配对
        let mut config = Config::new(dir.0.join("none.json"));
        config.enabled = true;
        config.lan_port = free_port();
        service.configure(config.clone());
        wait_for(|| service.status().enabled.then_some(())).unwrap();
        let status = service.status();
        assert!(!status.paired && status.lan_port == 0);
        assert!(!service.observe("a") && !service.observe("b"));
        // 设置界面配好对（写了配对文件）：自己跟着起来
        let mut state = State::fresh();
        state.group_key = Some([3u8; 32]);
        state.save(&config.state_file).unwrap();
        wait_for(|| (service.status().lan_port != 0).then_some(())).expect("配好对以后没起来");
        assert!(service.status().paired);
        // 关掉
        config.enabled = false;
        service.configure(config.clone());
        wait_for(|| (!service.status().enabled).then_some(())).unwrap();
        assert_eq!(service.status().lan_port, 0);
        assert!(!service.observe("c") && !service.observe("d"));
        // 配对文件坏了：报出来，不当成没配对悄悄过去
        std::fs::write(&config.state_file, "坏了").unwrap();
        config.enabled = true;
        service.configure(config);
        let error = wait_for(|| service.status().error).unwrap();
        assert!(error.contains("读不懂"), "{error}");
        service.shutdown();
    }

    #[test]
    fn copies_travel_through_the_relay() {
        if !test_support::available() {
            return;
        }
        let dir = Dir::new("relay");
        let server = test_support::start("relay-token").unwrap();
        let key_file = dir.0.join("relay.key");
        std::fs::write(&key_file, "relay-token\n").unwrap();
        let make = |name: &str| {
            let mut config = device(&dir, name, [8u8; 32]);
            // 不在同一个局域网：只有中转服务
            config.lan = false;
            config.relay_url = server.url.clone();
            config.relay_key_file = Some(key_file.clone());
            config
        };
        let (a, b) = (Service::new(), Service::new());
        a.configure(make("公司"));
        b.configure(make("家里"));
        a.touch();
        b.touch();
        for service in [&a, &b] {
            assert!(
                wait_for(|| (service.status().relay == RelayStatus::Ok).then_some(())).is_some(),
                "{:?}",
                service.status()
            );
        }
        assert!(!a.observe("基准"));
        assert!(a.observe("经中转服务过去的"));
        assert_eq!(
            wait_for(|| b.poll()),
            Some(Received {
                text: "经中转服务过去的".to_string(),
                from: "公司".to_string()
            })
        );
        // 自己发的那份中转服务也会回给自己：不收
        thread::sleep(Duration::from_millis(500));
        assert_eq!(a.poll(), None);
        assert_eq!(a.status().received, 0);
        // 中转服务上只有密文
        let relay = Relay::new(&server.url, "relay-token").unwrap();
        let channel = GroupKeys::derive(&[8u8; 32]).channel;
        let (_, stored) = relay.poll(&channel, Some(0), 0, None).unwrap();
        assert_eq!(stored.len(), 1);
        let needle = "经中转服务过去的".as_bytes();
        assert!(!stored[0].windows(needle.len()).any(|w| w == needle));
        assert!(!stored[0].windows(6).any(|w| w == "公司".as_bytes()));

        // 口令不对：报出来
        let wrong_key = dir.0.join("wrong.key");
        std::fs::write(&wrong_key, "nope\n").unwrap();
        let mut wrong = make("错口令");
        wrong.relay_key_file = Some(wrong_key);
        let c = Service::new();
        c.configure(wrong);
        let status = wait_for(|| match c.status().relay {
            RelayStatus::Error(why) => Some(why),
            _ => None,
        })
        .unwrap();
        assert!(status.contains("口令不对"), "{status}");
        // 没有口令文件
        let mut missing = make("没口令");
        missing.relay_key_file = Some(dir.0.join("missing.key"));
        let d = Service::new();
        d.configure(missing);
        wait_for(|| (d.status().relay == RelayStatus::NoToken).then_some(())).unwrap();
        for service in [a, b, c, d] {
            service.shutdown();
        }
    }

    #[test]
    fn same_copy_over_both_paths_arrives_once() {
        if !test_support::available() {
            return;
        }
        let dir = Dir::new("both");
        let server = test_support::start("t").unwrap();
        let key_file = dir.0.join("relay.key");
        std::fs::write(&key_file, "t\n").unwrap();
        let mut a = device(&dir, "甲", [4u8; 32]);
        let mut b = device(&dir, "乙", [4u8; 32]);
        a.extra_targets = vec![local(b.lan_port)];
        b.extra_targets = vec![local(a.lan_port)];
        for config in [&mut a, &mut b] {
            config.relay_url = server.url.clone();
            config.relay_key_file = Some(key_file.clone());
        }
        let (sa, sb) = (Service::new(), Service::new());
        sa.configure(a);
        sb.configure(b);
        sa.touch();
        sb.touch();
        wait_for(|| {
            let (x, y) = (sa.status(), sb.status());
            (x.lan_peers == 1
                && y.lan_peers == 1
                && x.relay == RelayStatus::Ok
                && y.relay == RelayStatus::Ok)
                .then_some(())
        })
        .expect("两条路没都通");
        assert!(!sa.observe("基准"));
        assert!(sa.observe("两条路都走"));
        assert_eq!(wait_for(|| sb.poll()).unwrap().text, "两条路都走");
        // 等中转服务那一份也到：还是只有一条
        thread::sleep(Duration::from_millis(1500));
        assert_eq!(sb.poll(), None);
        assert_eq!(sb.status().received, 1);
        sa.shutdown();
        sb.shutdown();
    }
}
