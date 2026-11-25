//! 不带 sync 特性编译时的跨设备剪贴板接口：`wowtype_sync_new` 返回 NULL，其余什么也不做。
//! 这样头文件不用分两套，外壳见到 NULL 就不接同步。

#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_int};

pub struct WowtypeSync;

/// 和头文件里的 `WowtypeSyncConfig` 一样。
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

static EMPTY: [u8; 1] = [0];
static EMPTY_JSON: &[u8] = b"{}\0";

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_new() -> *mut WowtypeSync {
    std::ptr::null_mut()
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_free(_sync: *mut WowtypeSync) {}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_configure(
    _sync: *mut WowtypeSync,
    _config: *const WowtypeSyncConfig,
) {
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_observe(
    _sync: *mut WowtypeSync,
    _text: *const c_char,
) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_touch(_sync: *mut WowtypeSync) {}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_poll(_sync: *mut WowtypeSync) -> *const c_char {
    std::ptr::null()
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_from(_sync: *const WowtypeSync) -> *const c_char {
    EMPTY.as_ptr() as *const c_char
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_status(_sync: *mut WowtypeSync) -> *const c_char {
    EMPTY_JSON.as_ptr() as *const c_char
}

// ---- 配对：都是「没有这个功能」

pub struct WowtypeSyncPairing;

/// 和头文件里的 `WowtypeSyncPairConfig` 一样。
#[repr(C)]
pub struct WowtypeSyncPairConfig {
    pub size: u32,
    pub state_file: *const c_char,
    pub device_name: *const c_char,
    pub lan: c_int,
    pub relay_url: *const c_char,
    pub relay_key_file: *const c_char,
}

static NOT_BUILT: &[u8] = "这个版本没带跨设备复制粘贴\0".as_bytes();

#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_pair_host(
    _config: *const WowtypeSyncPairConfig,
) -> *mut WowtypeSyncPairing {
    std::ptr::null_mut()
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_pair_join(
    _config: *const WowtypeSyncPairConfig,
    _code: *const c_char,
) -> *mut WowtypeSyncPairing {
    std::ptr::null_mut()
}
/// 3 = 没配上（WOWTYPE_SYNC_PAIR_FAILED）。
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_pairing_state(_pairing: *mut WowtypeSyncPairing) -> c_int {
    3
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_pairing_text(
    _pairing: *mut WowtypeSyncPairing,
) -> *const c_char {
    NOT_BUILT.as_ptr() as *const c_char
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_pairing_free(_pairing: *mut WowtypeSyncPairing) {}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_unpair(_state_file: *const c_char) -> c_int {
    -1
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_paired(_state_file: *const c_char) -> *const c_char {
    static EMPTY: &[u8] = b"{\"paired\":false,\"devices\":[]}\0";
    EMPTY.as_ptr() as *const c_char
}
#[no_mangle]
pub unsafe extern "C" fn wowtype_sync_ping(
    _relay_url: *const c_char,
    _relay_key_file: *const c_char,
) -> c_int {
    crate::ffi::set_last_error("这个版本没带跨设备复制粘贴");
    -1
}
