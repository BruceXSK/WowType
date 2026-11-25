//! 配对状态文件：这台设备的编号、组密钥、一起配过对的设备。
//!
//! 放在 `~/.config/wowtype/sync.json`（macOS、Windows 在数据目录），权限 0600，只有自己能读。
//! 设置界面（配对）写它，输入法读它；组密钥只存在这里，不进 `wowtype.conf`、不进日志。

use std::io;
use std::path::Path;

use serde_json::{json, Value};

use crate::crypto::{
    b64_decode, b64_encode, hex_decode, hex_encode, random, DeviceId, DEVICE_ID_LEN, KEY_LEN,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    pub name: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct State {
    pub device_id: DeviceId,
    /// 还没配对是 None。
    pub group_key: Option<[u8; KEY_LEN]>,
    /// 一起配过对的别的设备（只是给人看的；真正管用的是组密钥）。
    pub devices: Vec<Device>,
}

impl State {
    pub fn fresh() -> Self {
        Self {
            device_id: random(),
            group_key: None,
            devices: Vec::new(),
        }
    }

    pub fn paired(&self) -> bool {
        self.group_key.is_some()
    }

    /// 读状态文件。没有这个文件是「还没配对」；文件坏了报错，不悄悄当成没配对（免得把组密钥盖掉）。
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::fresh()),
            Err(e) => return Err(e),
        };
        Self::parse(&text).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("配对文件读不懂：{}", path.display()),
            )
        })
    }

    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let id = |v: &Value| -> Option<DeviceId> {
            hex_decode(v.as_str()?)?.try_into().ok()
        };
        let group_key = match value.get("group_key") {
            None | Some(Value::Null) => None,
            Some(v) => Some(<[u8; KEY_LEN]>::try_from(b64_decode(v.as_str()?)?).ok()?),
        };
        let devices = value
            .get("devices")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|d| {
                        Some(Device {
                            id: id(d.get("id")?)?,
                            name: d.get("name")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            device_id: id(value.get("device_id")?)?,
            group_key,
            devices,
        })
    }

    fn to_json(&self) -> String {
        let value = json!({
            "version": 1,
            "device_id": hex_encode(&self.device_id),
            "group_key": self.group_key.map(|k| b64_encode(&k)),
            "devices": self.devices.iter().map(|d| json!({
                "id": hex_encode(&d.id),
                "name": d.name,
            })).collect::<Vec<_>>(),
        });
        serde_json::to_string_pretty(&value).expect("序列化不会失败") + "\n"
    }

    /// 写状态文件：先写临时文件（0600）再改名，中途断电不会留下半截。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            create_private_dir(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, self.to_json().as_bytes())?;
        std::fs::rename(&tmp, path)
    }

    /// 记下一台配过对的设备（同一个编号的换成新名字）。自己不记。
    pub fn remember_device(&mut self, device: Device) {
        if device.id == self.device_id {
            return;
        }
        self.devices.retain(|d| d.id != device.id);
        self.devices.push(device);
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // 以前留下的临时文件权限可能更松
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

/// 读只有一行的密钥文件（中转服务的访问口令）；没有或为空返回 None。
pub fn read_secret_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

const _: () = assert!(DEVICE_ID_LEN == 8);

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wowtype-sync-store-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_and_load() {
        let dir = temp("roundtrip");
        let path = dir.join("nested").join("sync.json");
        assert!(!State::load(&path).unwrap().paired(), "没有文件是还没配对");
        let mut state = State::fresh();
        state.group_key = Some([9u8; 32]);
        state.remember_device(Device {
            id: [2u8; 8],
            name: "书房的电脑".to_string(),
        });
        state.remember_device(Device {
            id: [2u8; 8],
            name: "书房".to_string(),
        });
        state.remember_device(Device {
            id: state.device_id,
            name: "自己".to_string(),
        });
        assert_eq!(state.devices.len(), 1, "同一台只记一次，自己不记");
        state.save(&path).unwrap();
        let loaded = State::load(&path).unwrap();
        assert!(loaded == state);
        assert_eq!(loaded.devices[0].name, "书房");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "配对文件只有自己能读");
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        std::fs::write(&path, "{ 坏了").unwrap();
        assert!(State::load(&path).is_err(), "坏文件要报错，不当成没配对");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_line() {
        let dir = temp("secret");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("relay.key");
        assert_eq!(read_secret_line(&path), None);
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_secret_line(&path), None);
        std::fs::write(&path, " token-123 \nmore\n").unwrap();
        assert_eq!(read_secret_line(&path).as_deref(), Some("token-123"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
