//! 配对的完整流程，和给设置界面调的命令行（`wowtype-sync`）。
//!
//! 配对在设置界面那个进程里做（输入法只管同步）：配好了把组密钥写进配对文件，输入法盯着这个文件，
//! 看到变了自己跟上。命令行一行一条结果，设置界面照着读：
//!
//! ```text
//! wowtype-sync pair-host --state 文件 --name 设备名 [--relay 地址 --relay-key-file 文件] [--no-lan]
//!     → code 123456 300        显示这个配对码，300 秒内有效
//!     → paired 另一台的名字     配好了（退出码 0）
//!     → error 说明              没配上（退出码 1）
//! wowtype-sync pair-join 123456 --state 文件 --name 设备名 [--relay …] [--no-lan] [--host 地址:端口]
//!     → paired … / error …
//! wowtype-sync status --state 文件     → 一行 JSON
//! wowtype-sync unpair --state 文件     → ok
//! wowtype-sync ping --relay 地址 --relay-key-file 文件   → ok / error …
//! ```
//!
//! 排查问题、测试用的两条（不经输入法，直接拿配对文件里的组密钥收发）：
//!
//! ```text
//! wowtype-sync listen --state 文件 [--port 端口] [--peer 地址:端口]… [--relay …] [--seconds 秒]
//!     → ready 端口             开始听了
//!     → recv 设备名<TAB>内容   每收到一条一行（内容里的换行写成 \n）
//! wowtype-sync send 文字 --state 文件 --name 设备名 --host 地址:端口   直接送给这个地址 → ok
//! ```
//!
//! 访问口令只从文件读，不走命令行参数（`ps` 看得到参数）。

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::crypto::{hex_encode, random, KEY_LEN};
use crate::lan::{self, TcpChannel};
use crate::pairing::{self, Channel, HostCode, PairError};
use crate::relay::{Canceller, MailChannel, Relay};
use crate::store::{read_secret_line, Device, State};

#[derive(Clone, Debug)]
pub struct PairOptions {
    pub state_file: PathBuf,
    /// 这台设备叫什么。
    pub name: String,
    pub relay: Option<Relay>,
    pub lan: bool,
    /// 局域网发现用的端口，一般是 [`lan::PORT`]。
    pub lan_port: u16,
    /// 加入时不靠广播找、直接连这个地址（广播被防火墙挡了的时候）。
    pub host: Option<SocketAddr>,
}

impl PairOptions {
    pub fn new(state_file: PathBuf, name: &str) -> Self {
        Self {
            state_file,
            name: name.to_string(),
            relay: None,
            lan: true,
            lan_port: lan::PORT,
            host: None,
        }
    }
}

fn load(options: &PairOptions) -> Result<(State, Device), String> {
    let state = State::load(&options.state_file).map_err(|e| e.to_string())?;
    let name: String = options
        .name
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    let me = Device {
        id: state.device_id,
        name: if name.trim().is_empty() {
            "WowType".to_string()
        } else {
            name
        },
    };
    Ok((state, me))
}

/// 主持配对：生成配对码交给 `on_code`（显示出来），等另一台设备输入。配好了把对方记进配对文件、返回对方。
/// `on_code` 的第二个参数是配对时另一台可以直接连的地址（局域网开着时）。`cancel` 置位就停。
pub fn pair_host(
    options: &PairOptions,
    cancel: &AtomicBool,
    on_code: &mut dyn FnMut(&str, Option<SocketAddr>),
) -> Result<Device, String> {
    let (mut state, me) = load(options)?;
    if !options.lan && options.relay.is_none() {
        return Err("局域网和中转服务至少要开一个".to_string());
    }
    // 还没有组密钥的话这次新建一把；配成了才存，没配成不留下只有自己的「组」
    let group_key: [u8; KEY_LEN] = state.group_key.unwrap_or_else(random);
    let code = Mutex::new(HostCode::new());
    let joined: Mutex<Option<Device>> = Mutex::new(None);
    let done = AtomicBool::new(false);
    let canceller = Canceller::default();

    let lan = if options.lan {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|l| l.set_nonblocking(true).map(|_| l))
            .map_err(|e| format!("局域网端口打不开：{e}"))?;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|s| s.set_broadcast(true).map(|_| s))
            .map_err(|e| format!("局域网广播发不出去：{e}"))?;
        Some((listener, socket))
    } else {
        None
    };
    let port = lan
        .as_ref()
        .and_then(|(listener, _)| listener.local_addr().ok())
        .map(|a| a.port());
    let address = port.and_then(|port| lan::local_ip().map(|ip| SocketAddr::new(ip, port)));
    on_code(&code.lock().unwrap().code.clone(), address);

    let serve = |channel: &mut dyn Channel, hello: &[u8]| {
        let result = code.lock().unwrap().serve(channel, hello, &group_key, &me);
        if let Ok(device) = result {
            *joined.lock().unwrap() = Some(device);
            done.store(true, Ordering::SeqCst);
        }
    };
    let finished = || {
        done.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) || !code.lock().unwrap().usable()
    };

    std::thread::scope(|scope| {
        if let Some(relay) = &options.relay {
            let (serve, finished, canceller) = (&serve, &finished, &canceller);
            scope.spawn(move || {
                // 上一次没走完的留言清掉
                let _ = relay.clear_mail();
                let mut channel = MailChannel::host(relay, Some(canceller));
                while !finished() {
                    match channel.recv(Duration::from_secs(20)) {
                        Ok(message) if pairing::is_hello(&message) => serve(&mut channel, &message),
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                        // 中转服务连不上：歇一下再试，局域网那条路照常
                        Err(_) => std::thread::sleep(Duration::from_secs(2)),
                    }
                }
            });
        }
        let mut last_offer: Option<Instant> = None;
        while !finished() {
            let Some((listener, socket)) = &lan else {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            if last_offer.map_or(true, |at| at.elapsed() >= Duration::from_secs(1)) {
                let offer = lan::offer(port.unwrap_or(0), &me.name);
                for target in lan::broadcast_targets(options.lan_port) {
                    let _ = socket.send_to(&offer, target);
                }
                last_offer = Some(Instant::now());
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let mut channel = TcpChannel(stream);
                    if let Ok(hello) = channel.recv(Duration::from_secs(5)) {
                        serve(&mut channel, &hello);
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        done.store(true, Ordering::SeqCst);
        canceller.cancel();
    });

    let Some(device) = joined.into_inner().unwrap() else {
        return Err(if cancel.load(Ordering::SeqCst) {
            "已取消".to_string()
        } else if code.lock().unwrap().time_left().is_zero() {
            "配对码过期了，重新来一次".to_string()
        } else {
            "配对码输错太多次，重新来一次".to_string()
        });
    };
    state.group_key = Some(group_key);
    state.remember_device(device.clone());
    state
        .save(&options.state_file)
        .map_err(|e| format!("配对文件写不进去：{e}"))?;
    Ok(device)
}

/// 加入：拿另一台设备上显示的配对码换组密钥，写进配对文件。返回主持的那台设备。
pub fn pair_join(options: &PairOptions, code: &str) -> Result<Device, String> {
    let code = pairing::normalize_code(code).ok_or("配对码是 6 位数字")?;
    let (mut state, me) = load(options)?;
    let mut wrong = false;
    let mut result = None;

    let mut hosts: Vec<SocketAddr> = options.host.into_iter().collect();
    if hosts.is_empty() && options.lan {
        if let Ok(socket) = lan::bind_udp(options.lan_port) {
            hosts = lan::discover_offers(&socket, Duration::from_secs(3))
                .into_iter()
                .map(|(addr, _)| addr)
                .collect();
        }
    }
    let found_host = !hosts.is_empty();
    for addr in hosts {
        let Ok(mut channel) = TcpChannel::connect(addr) else {
            continue;
        };
        match pairing::join(&mut channel, &code, &me) {
            Ok(pair) => {
                result = Some(pair);
                break;
            }
            // 同一个网里可能还有别人也在配对：这台不对，接着试下一台
            Err(PairError::WrongCode) => wrong = true,
            Err(_) => {}
        }
    }
    let mut relay_error = None;
    if result.is_none() {
        if let Some(relay) = &options.relay {
            match pairing::join(&mut MailChannel::join(relay, None), &code, &me) {
                Ok(pair) => result = Some(pair),
                Err(PairError::WrongCode) => wrong = true,
                Err(PairError::Timeout) => {}
                Err(e) => relay_error = Some(e.to_string()),
            }
        }
    }
    let Some((group_key, host)) = result else {
        return Err(if wrong {
            "配对码不对".to_string()
        } else if let Some(error) = relay_error {
            error
        } else if found_host || options.relay.is_some() {
            "另一台设备没有回应：它要正显示着配对码".to_string()
        } else {
            "没找到正在配对的设备：两台设备要在同一个局域网里，另一台要正显示着配对码".to_string()
        });
    };
    if state.group_key != Some(group_key) {
        // 换了一组：以前那组的设备不再是一起的了
        state.devices.clear();
    }
    state.group_key = Some(group_key);
    state.remember_device(host.clone());
    state
        .save(&options.state_file)
        .map_err(|e| format!("配对文件写不进去：{e}"))?;
    Ok(host)
}

/// 取消配对：这台设备忘掉组密钥和别的设备（设备编号留着）。别的设备之间不受影响。
pub fn unpair(state_file: &std::path::Path) -> Result<(), String> {
    let mut state = State::load(state_file).unwrap_or_else(|_| State::fresh());
    state.group_key = None;
    state.devices.clear();
    state.save(state_file).map_err(|e| e.to_string())
}

/// 配对文件里有什么（不含密钥），给设置界面看。
pub fn status_json(state_file: &std::path::Path) -> String {
    match State::load(state_file) {
        Ok(state) => json!({
            "paired": state.paired(),
            "device_id": hex_encode(&state.device_id),
            "devices": state.devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
        }),
        Err(e) => json!({ "paired": false, "devices": [], "error": e.to_string() }),
    }
    .to_string()
}

fn usage() -> String {
    "用法：wowtype-sync pair-host|pair-join <配对码>|status|unpair|ping|listen|send <文字> \
     [--state 文件] [--name 设备名] [--relay 地址] [--relay-key-file 文件] [--no-lan] [--host 地址:端口]"
        .to_string()
}

/// `listen`：起一份同步服务，把收到的打出来，直到过了 `seconds`（None 一直听）。
fn listen(config: crate::service::Config, seconds: Option<u64>, say: &mut dyn FnMut(String)) -> i32 {
    let service = crate::service::Service::new();
    service.configure(config);
    let started = Instant::now();
    let mut ready = false;
    while seconds.map_or(true, |s| started.elapsed() < Duration::from_secs(s)) {
        // 一直当作有人在用：中转服务那边保持长轮询
        service.touch();
        let status = service.status();
        if !ready && status.enabled {
            if !status.paired {
                say(format!(
                    "error {}",
                    status.error.unwrap_or_else(|| "这台设备还没有配对".to_string())
                ));
                service.shutdown();
                return 1;
            }
            if !status.lan || status.lan_port != 0 || status.lan_error.is_some() {
                say(format!("ready {}", status.lan_port));
                ready = true;
            }
        }
        while let Some(received) = service.poll() {
            say(format!(
                "recv {}\t{}",
                received.from,
                received.text.replace('\\', "\\\\").replace('\n', "\\n")
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    service.shutdown();
    0
}

/// `send`：拿配对文件里的组密钥封一个信封，直接送到 `host`。
fn send(state_file: &std::path::Path, name: &str, host: SocketAddr, text: &str) -> Result<(), String> {
    let state = State::load(state_file).map_err(|e| e.to_string())?;
    let key = state.group_key.ok_or("这台设备还没有配对")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let payload = json!({ "text": text, "name": name }).to_string();
    let envelope = crate::crypto::GroupKeys::derive(&key).seal(&state.device_id, now, payload.as_bytes());
    lan::send_frame(host, &envelope).map_err(|e| format!("送不过去：{e}"))
}

/// 命令行入口。`args` 不含程序名。返回退出码。
pub fn run(args: &[String], out: &mut dyn Write) -> i32 {
    let mut say = |line: String| {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    let Some(command) = args.first().map(String::as_str) else {
        say(usage());
        return 2;
    };
    let mut positional = Vec::new();
    let mut state_file = None;
    let mut name = String::new();
    let mut relay_url = None;
    let mut relay_key_file = None;
    let mut lan = true;
    let mut host = None;
    let mut port = None;
    let mut peers = Vec::new();
    let mut seconds = None;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let mut value = |what: &str| rest.next().cloned().ok_or(format!("{what} 后面要跟一个值"));
        let parsed = match arg.as_str() {
            "--state" => value(arg).map(|v| state_file = Some(PathBuf::from(v))),
            "--name" => value(arg).map(|v| name = v),
            "--relay" => value(arg).map(|v| relay_url = Some(v)),
            "--relay-key-file" => value(arg).map(|v| relay_key_file = Some(PathBuf::from(v))),
            "--host" => value(arg).and_then(|v| {
                v.parse::<SocketAddr>()
                    .map(|addr| host = Some(addr))
                    .map_err(|_| "--host 要写成 地址:端口".to_string())
            }),
            "--peer" => value(arg).and_then(|v| {
                v.parse::<SocketAddr>()
                    .map(|addr| peers.push(addr))
                    .map_err(|_| "--peer 要写成 地址:端口".to_string())
            }),
            "--port" => value(arg).and_then(|v| {
                v.parse::<u16>()
                    .map(|p| port = Some(p))
                    .map_err(|_| "--port 要是端口号".to_string())
            }),
            "--seconds" => value(arg).and_then(|v| {
                v.parse::<u64>()
                    .map(|s| seconds = Some(s))
                    .map_err(|_| "--seconds 要是秒数".to_string())
            }),
            "--no-lan" => {
                lan = false;
                Ok(())
            }
            other if other.starts_with("--") => Err(format!("不认识的参数 {other}")),
            other => {
                positional.push(other.to_string());
                Ok(())
            }
        };
        if let Err(error) = parsed {
            say(format!("error {error}"));
            return 2;
        }
    }
    if command == "listen" {
        let Some(state_file) = state_file else {
            say("error 要用 --state 指明配对文件".to_string());
            return 2;
        };
        let mut config = crate::service::Config::new(state_file);
        config.enabled = true;
        config.lan = lan;
        config.device_name = name;
        config.relay_url = relay_url.unwrap_or_default();
        config.relay_key_file = relay_key_file;
        config.lan_port = port.unwrap_or(lan::PORT);
        if !peers.is_empty() {
            config.beacon_interval = Duration::from_millis(200);
        }
        config.extra_targets = peers;
        return listen(config, seconds, &mut say);
    }
    let relay = match relay_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        None => None,
        Some(url) => {
            let Some(token) = relay_key_file.as_deref().and_then(read_secret_line) else {
                say("error 还没有填中转服务的访问口令".to_string());
                return 1;
            };
            match Relay::new(url, &token) {
                Ok(relay) => Some(relay),
                Err(e) => {
                    say(format!("error {e}"));
                    return 1;
                }
            }
        }
    };
    if command == "ping" {
        return match relay.as_ref().map(Relay::ping) {
            Some(Ok(())) => {
                say("ok".to_string());
                0
            }
            Some(Err(e)) => {
                say(format!("error {e}"));
                1
            }
            None => {
                say("error 还没有填中转服务的地址".to_string());
                1
            }
        };
    }
    let Some(state_file) = state_file else {
        say("error 要用 --state 指明配对文件".to_string());
        return 2;
    };
    let mut options = PairOptions::new(state_file.clone(), &name);
    options.relay = relay;
    options.lan = lan;
    options.host = host;
    let outcome = match command {
        "status" => {
            say(status_json(&state_file));
            return 0;
        }
        "unpair" => unpair(&state_file).map(|_| "ok".to_string()),
        "pair-host" => {
            let cancel = AtomicBool::new(false);
            let mut announce = |code: &str, address: Option<SocketAddr>| {
                let address = address.map_or(String::new(), |a| format!(" {a}"));
                say(format!("code {code} {}{address}", pairing::CODE_TTL.as_secs()));
            };
            pair_host(&options, &cancel, &mut announce).map(|d| format!("paired {}", d.name))
        }
        "send" => match (positional.first(), host) {
            (Some(text), Some(host)) => send(&state_file, &name, host, text).map(|_| "ok".to_string()),
            _ => Err("要给文字和 --host 地址:端口".to_string()),
        },
        "pair-join" => match positional.first() {
            Some(code) => pair_join(&options, code).map(|d| format!("paired {}", d.name)),
            None => Err("要给配对码".to_string()),
        },
        _ => {
            say(usage());
            return 2;
        }
    };
    match outcome {
        Ok(line) => {
            say(line);
            0
        }
        Err(error) => {
            say(format!("error {error}"));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::test_support;
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wowtype-sync-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 起一个主持方，返回（配对码, 配对地址, 结果）。
    fn host(
        options: PairOptions,
    ) -> (
        String,
        Option<SocketAddr>,
        std::thread::JoinHandle<Result<Device, String>>,
        Arc<AtomicBool>,
    ) {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let thread = std::thread::spawn(move || {
            pair_host(&options, &stop, &mut |code, address| {
                tx.send((code.to_string(), address)).unwrap();
            })
        });
        let (code, address) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        (code, address, thread, cancel)
    }

    fn loopback(address: Option<SocketAddr>) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, address.unwrap().port()))
    }

    #[test]
    fn pair_over_lan_then_a_third_device_joins() {
        let dir = temp("lan");
        let a = PairOptions::new(dir.join("a.json"), "书房");
        let (code, address, hosting, _) = host(a.clone());
        // 输错一次不要紧
        let mut b = PairOptions::new(dir.join("b.json"), "客厅");
        b.host = Some(loopback(address));
        let wrong = if code == "000000" { "000001" } else { "000000" };
        assert_eq!(pair_join(&b, wrong).unwrap_err(), "配对码不对");
        assert!(!State::load(&b.state_file).unwrap().paired());
        assert_eq!(pair_join(&b, &code).unwrap().name, "书房");
        assert_eq!(hosting.join().unwrap().unwrap().name, "客厅");

        let (sa, sb) = (
            State::load(&a.state_file).unwrap(),
            State::load(&b.state_file).unwrap(),
        );
        assert!(sa.paired() && sa.group_key == sb.group_key);
        assert_eq!(sa.devices[0].name, "客厅");
        assert_eq!(sb.devices[0].name, "书房");
        assert_ne!(sa.device_id, sb.device_id);

        // 第三台找已经在组里的任何一台配，拿到的是同一把组密钥
        let (code, address, hosting, _) = host(b.clone());
        let mut c = PairOptions::new(dir.join("c.json"), "笔记本");
        c.host = Some(loopback(address));
        pair_join(&c, &format!("{} {}", &code[..3], &code[3..])).unwrap();
        hosting.join().unwrap().unwrap();
        assert_eq!(State::load(&c.state_file).unwrap().group_key, sa.group_key);
        assert_eq!(State::load(&b.state_file).unwrap().devices.len(), 2);

        // 取消配对只动自己
        unpair(&c.state_file).unwrap();
        let sc = State::load(&c.state_file).unwrap();
        assert!(!sc.paired() && sc.devices.is_empty());
        assert!(State::load(&a.state_file).unwrap().paired());
        assert!(status_json(&a.state_file).contains("\"paired\":true"));
        assert!(!status_json(&a.state_file).contains("group_key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_gives_up_when_cancelled_and_saves_nothing() {
        let dir = temp("cancel");
        let options = PairOptions::new(dir.join("a.json"), "书房");
        let (_code, _address, hosting, cancel) = host(options.clone());
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(hosting.join().unwrap().unwrap_err(), "已取消");
        assert!(!options.state_file.exists(), "没配成不留下组密钥");
        // 没人主持时加入：说清楚为什么
        let mut join = PairOptions::new(dir.join("b.json"), "客厅");
        join.host = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 9)));
        assert!(pair_join(&join, "123456").unwrap_err().contains("没有回应"));
        assert_eq!(pair_join(&join, "12345").unwrap_err(), "配对码是 6 位数字");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pair_through_the_relay() {
        if !test_support::available() {
            return;
        }
        let dir = temp("relay");
        let server = test_support::start("t").unwrap();
        let relay = Relay::new(&server.url, "t").unwrap();
        let mut a = PairOptions::new(dir.join("a.json"), "公司");
        a.lan = false;
        a.relay = Some(relay.clone());
        let mut b = PairOptions::new(dir.join("b.json"), "家里");
        b.lan = false;
        b.relay = Some(relay);
        let (code, address, hosting, _) = host(a.clone());
        assert_eq!(address, None, "没开局域网就没有直连地址");
        assert_eq!(pair_join(&b, &code).unwrap().name, "公司");
        assert_eq!(hosting.join().unwrap().unwrap().name, "家里");
        assert_eq!(
            State::load(&a.state_file).unwrap().group_key,
            State::load(&b.state_file).unwrap().group_key
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listen_prints_what_send_delivers() {
        let dir = temp("listen");
        let mut state = State::fresh();
        state.group_key = Some([3u8; 32]);
        let (a, b) = (dir.join("a.json"), dir.join("b.json"));
        state.save(&a).unwrap();
        state.device_id = [7u8; 8];
        state.save(&b).unwrap();
        let port = {
            let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let listening = std::thread::spawn(move || {
            let mut config = crate::service::Config::new(a);
            config.enabled = true;
            config.lan_port = port;
            listen(config, Some(4), &mut |line| sink.lock().unwrap().push(line))
        });
        let wait = |wanted: usize| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while lines.lock().unwrap().len() < wanted && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            lines.lock().unwrap().clone()
        };
        assert_eq!(wait(1), vec![format!("ready {port}")]);
        send(&b, "另一台", SocketAddr::from((Ipv4Addr::LOCALHOST, port)), "第一行\n第二行").unwrap();
        assert_eq!(wait(2)[1], "recv 另一台\t第一行\\n第二行");
        assert_eq!(listening.join().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_line() {
        let dir = temp("cmd");
        let state = dir.join("s.json").to_string_lossy().into_owned();
        let call = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let mut out = Vec::new();
            let code = run(&args, &mut out);
            (code, String::from_utf8(out).unwrap())
        };
        assert_eq!(call(&[]).0, 2);
        assert_eq!(call(&["nonsense", "--state", &state]).0, 2);
        assert_eq!(call(&["status"]).0, 2);
        assert_eq!(call(&["status", "--wat"]), (2, "error 不认识的参数 --wat\n".to_string()));
        let (code, out) = call(&["status", "--state", &state]);
        assert_eq!(code, 0);
        assert!(out.contains("\"paired\":false"), "{out}");
        assert_eq!(call(&["unpair", "--state", &state]), (0, "ok\n".to_string()));
        assert_eq!(
            call(&["pair-join", "--state", &state]),
            (1, "error 要给配对码\n".to_string())
        );
        assert_eq!(
            call(&["pair-join", "abc", "--state", &state, "--no-lan"]),
            (1, "error 配对码是 6 位数字\n".to_string())
        );
        assert_eq!(
            call(&["pair-host", "--state", &state, "--no-lan"]),
            (1, "error 局域网和中转服务至少要开一个\n".to_string())
        );
        assert_eq!(
            call(&["ping"]),
            (1, "error 还没有填中转服务的地址\n".to_string())
        );
        assert_eq!(
            call(&["ping", "--relay", "https://example.com"]),
            (1, "error 还没有填中转服务的访问口令\n".to_string())
        );
        let key = dir.join("k");
        std::fs::write(&key, "t\n").unwrap();
        let key = key.to_string_lossy().into_owned();
        assert_eq!(
            call(&["ping", "--relay", "http://example.com", "--relay-key-file", &key]),
            (1, "error 中转服务的地址要以 https:// 开头\n".to_string())
        );
        assert_eq!(call(&["pair-join", "1", "--host", "nope"]).0, 2);
        // listen / send：没配对的说清楚
        let (code, out) = call(&["listen", "--state", &state, "--port", "0", "--seconds", "5"]);
        assert_eq!((code, out.as_str()), (1, "error 这台设备还没有配对\n"));
        assert_eq!(
            call(&["send", "hi", "--state", &state, "--host", "127.0.0.1:9"]),
            (1, "error 这台设备还没有配对\n".to_string())
        );
        assert_eq!(call(&["send", "hi", "--state", &state]).0, 1);
        if test_support::available() {
            let server = test_support::start("t").unwrap();
            assert_eq!(
                call(&["ping", "--relay", &server.url, "--relay-key-file", &key]),
                (0, "ok\n".to_string())
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
