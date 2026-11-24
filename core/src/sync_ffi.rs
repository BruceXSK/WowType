//! 跨设备剪贴板的 C 接口（ABI 1.19）。逻辑在 `wowtype-sync`，这里只是包一层。
//!
//! 外壳定时把本机剪贴板交给 `wowtype_sync_observe`、定时 `wowtype_sync_poll`；这些函数都不阻塞，
//! 网络、加密都在 `wowtype-sync` 自己的线程里。

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;

use wowtype_sync::{Config, Service};

pub struct WowtypeSync {
    service: Service,
    text: CString,
    from: CString,
    status: CString,
}

/// 和头文件里的 `WowtypeSyncConfig` 一样；只在末尾加字段。
#[repr(C)]
pub struct WowtypeSyncConfig {
    pub size: u32,
    pub enabled: c_int,
    pub lan: c_int,
    pub relay_url: *const c_char,
    pub relay_key_file: *const c_char,
    pub state_file: *const c_char,
    pub device_name: *const c_char,
}

unsafe fn arg(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().trim().to_string()
    }
}

/// 中间有 NUL 的（不会有，别的设备传来的也过滤过）截到 NUL 之前。
fn c_string(text: &str) -> CString {
    let end = text.find('\0').unwrap_or(text.len());
    CString::new(&text[..end]).unwrap_or_default()
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_new() -> *mut WowtypeSync {
    Box::into_raw(Box::new(WowtypeSync {
        service: Service::new(),
        text: CString::default(),
        from: CString::default(),
        status: CString::default(),
    }))
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_free(sync: *mut WowtypeSync) {
    if !sync.is_null() {
        drop(Box::from_raw(sync));
    }
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_configure(
    sync: *mut WowtypeSync,
    config: *const WowtypeSyncConfig,
) {
    let (Some(sync), Some(raw)) = (sync.as_ref(), config.as_ref()) else {
        return;
    };
    if (raw.size as usize) < std::mem::size_of::<WowtypeSyncConfig>() {
        return;
    }
    let state_file = arg(raw.state_file);
    let mut config = Config::new(crate::llama::expand_home(&state_file));
    // 没给配对文件就没法配对、也没有密钥：当成关着
    config.enabled = raw.enabled != 0 && !state_file.is_empty();
    config.lan = raw.lan != 0;
    config.relay_url = arg(raw.relay_url);
    let key_file = arg(raw.relay_key_file);
    config.relay_key_file =
        (!key_file.is_empty()).then(|| PathBuf::from(crate::llama::expand_home(&key_file)));
    config.device_name = arg(raw.device_name);
    test_knobs(&mut config);
    sync.service.configure(config);
}

/// 测试、排查用的环境变量（外壳的按键测试拿它把两台「设备」放在同一台机器上）：
/// `WOWTYPE_SYNC_LAN_PORT` 换局域网端口，`WOWTYPE_SYNC_PEERS`（逗号分开的 `地址:端口`）除了广播
/// 还往这些地址发，`WOWTYPE_SYNC_BEACON_MS` 改广播间隔。
fn test_knobs(config: &mut Config) {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    if let Some(port) = env("WOWTYPE_SYNC_LAN_PORT").and_then(|v| v.trim().parse().ok()) {
        config.lan_port = port;
    }
    if let Some(peers) = env("WOWTYPE_SYNC_PEERS") {
        config.extra_targets = peers
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
    }
    if let Some(ms) = env("WOWTYPE_SYNC_BEACON_MS").and_then(|v| v.trim().parse().ok()) {
        config.beacon_interval = std::time::Duration::from_millis(ms);
    }
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_observe(sync: *mut WowtypeSync, text: *const c_char) -> c_int {
    let Some(sync) = sync.as_ref() else {
        return 0;
    };
    if text.is_null() {
        return 0;
    }
    // 不是 UTF-8 的不发（to_string_lossy 会把它改掉，对面拿到的就不是原文了）
    match CStr::from_ptr(text).to_str() {
        Ok(text) => sync.service.observe(text) as c_int,
        Err(_) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_touch(sync: *mut WowtypeSync) {
    if let Some(sync) = sync.as_ref() {
        sync.service.touch();
    }
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_poll(sync: *mut WowtypeSync) -> *const c_char {
    let Some(sync) = sync.as_mut() else {
        return std::ptr::null();
    };
    let Some(received) = sync.service.poll() else {
        return std::ptr::null();
    };
    // 工具栏的「剪贴板」里看得到，输入法自己就能贴，不依赖系统剪贴板写不写得进去
    crate::clipboard::remember(&received.text);
    sync.text = c_string(&received.text);
    sync.from = c_string(&received.from);
    sync.text.as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_from(sync: *const WowtypeSync) -> *const c_char {
    static EMPTY: [u8; 1] = [0];
    match sync.as_ref() {
        Some(sync) => sync.from.as_ptr(),
        None => EMPTY.as_ptr() as *const c_char,
    }
}

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_status(sync: *mut WowtypeSync) -> *const c_char {
    static EMPTY: &[u8] = b"{}\0";
    let Some(sync) = sync.as_mut() else {
        return EMPTY.as_ptr() as *const c_char;
    };
    sync.status = c_string(&sync.service.status().to_json());
    sync.status.as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn c(text: &str) -> CString {
        CString::new(text).unwrap()
    }

    #[test]
    fn null_and_unconfigured_are_harmless() {
        unsafe {
            wowtype_sync_free(std::ptr::null_mut());
            wowtype_sync_configure(std::ptr::null_mut(), std::ptr::null());
            assert_eq!(wowtype_sync_observe(std::ptr::null_mut(), c("x").as_ptr()), 0);
            wowtype_sync_touch(std::ptr::null_mut());
            assert!(wowtype_sync_poll(std::ptr::null_mut()).is_null());
            assert_eq!(CStr::from_ptr(wowtype_sync_from(std::ptr::null())).to_bytes(), b"");
            assert_eq!(CStr::from_ptr(wowtype_sync_status(std::ptr::null_mut())).to_bytes(), b"{}");

            let sync = wowtype_sync_new();
            wowtype_sync_configure(sync, std::ptr::null());
            assert_eq!(wowtype_sync_observe(sync, std::ptr::null()), 0);
            assert_eq!(wowtype_sync_observe(sync, c("a").as_ptr()), 0);
            assert_eq!(wowtype_sync_observe(sync, c("b").as_ptr()), 0, "没打开不发");
            assert!(wowtype_sync_poll(sync).is_null());
            let status = CStr::from_ptr(wowtype_sync_status(sync)).to_str().unwrap();
            assert!(status.contains("\"enabled\":false"), "{status}");
            wowtype_sync_free(sync);
        }
    }

    #[test]
    fn two_handles_sync_and_fill_the_clipboard_history() {
        let _clipboard = crate::clipboard::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::clipboard::clear();
        let dir = std::env::temp_dir().join(format!("wowtype-sync-ffi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 两台配好对的设备：同一把组密钥
        let mut services = Vec::new();
        let mut ports = Vec::new();
        for name in ["a", "b"] {
            let state_file = dir.join(format!("{name}.json"));
            let mut state = wowtype_sync::store::State::fresh();
            state.group_key = Some([6u8; 32]);
            state.save(&state_file).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            ports.push(listener.local_addr().unwrap().port());
            services.push(state_file);
        }
        unsafe {
            let handles: Vec<*mut WowtypeSync> = (0..2).map(|_| wowtype_sync_new()).collect();
            for (i, handle) in handles.iter().enumerate() {
                // C 接口没有测试用的旋钮（端口、额外的广播地址）：直接配里面的服务
                let mut config = Config::new(services[i].clone());
                config.enabled = true;
                config.device_name = ["书房", "客厅"][i].to_string();
                config.lan_port = ports[i];
                config.beacon_interval = Duration::from_millis(100);
                config.extra_targets = vec![format!("127.0.0.1:{}", ports[1 - i]).parse().unwrap()];
                (**handle).service.configure(config);
            }
            let wait = |probe: &mut dyn FnMut() -> bool| {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !probe() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                probe()
            };
            assert!(wait(&mut || {
                handles.iter().all(|h| {
                    CStr::from_ptr(wowtype_sync_status(*h))
                        .to_str()
                        .unwrap()
                        .contains("\"lan_peers\":1")
                })
            }));
            assert_eq!(wowtype_sync_observe(handles[0], c("原有的").as_ptr()), 0);
            assert_eq!(wowtype_sync_observe(handles[0], c("跨设备的一句").as_ptr()), 1);
            let mut got: *const c_char = std::ptr::null();
            assert!(wait(&mut || {
                if got.is_null() {
                    got = wowtype_sync_poll(handles[1]);
                }
                !got.is_null()
            }));
            assert_eq!(CStr::from_ptr(got).to_str().unwrap(), "跨设备的一句");
            assert_eq!(CStr::from_ptr(wowtype_sync_from(handles[1])).to_str().unwrap(), "书房");
            assert_eq!(
                crate::clipboard::history(),
                vec!["跨设备的一句".to_string()],
                "收到的记进剪贴板历史"
            );
            // 外壳把它写进系统剪贴板再报回来：不往回发
            assert_eq!(wowtype_sync_observe(handles[1], c("原有的 b").as_ptr()), 0);
            assert_eq!(wowtype_sync_observe(handles[1], c("跨设备的一句").as_ptr()), 0);
            for handle in handles {
                (*handle).service.shutdown();
                wowtype_sync_free(handle);
            }
        }
        crate::clipboard::clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_struct_matches_the_header() {
        let header = include_str!("../include/wowtype_core.h");
        for field in [
            "uint32_t size;",
            "int enabled;",
            "int lan;",
            "const char *relay_url;",
            "const char *relay_key_file;",
            "const char *state_file;",
            "const char *device_name;",
        ] {
            assert!(header.contains(field), "头文件里没有 {field}");
        }
        // 字段顺序和头文件一样
        let order: Vec<usize> = ["int enabled;", "int lan;", "*relay_url;", "*relay_key_file;", "*state_file;", "*device_name;"]
            .iter()
            .map(|f| header.find(f).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
    }
}
