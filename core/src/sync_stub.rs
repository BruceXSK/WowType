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
