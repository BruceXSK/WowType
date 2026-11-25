//! 密钥和信封：一组设备共用一把 32 字节的组密钥，剪贴板内容用它派生出的密钥端到端加密。
//!
//! 局域网和中转服务传的是同一种信封，中转服务和同一个网里的别人都只看得到密文。

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const KEY_LEN: usize = 32;
pub const DEVICE_ID_LEN: usize = 8;
pub const NONCE_LEN: usize = 24;

const MAGIC: &[u8; 4] = b"WTS1";
/// 信封头：魔数 4 + 种类 1 + 发送设备 8 + 时间戳 8。
const HEADER_LEN: usize = 4 + 1 + DEVICE_ID_LEN + 8;
/// 信封种类：剪贴板内容。
const KIND_CLIP: u8 = 1;

pub type DeviceId = [u8; DEVICE_ID_LEN];

/// 系统的随机数。取不到（极少见）就 panic：没有随机数不能假装加了密。
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).expect("系统随机数不可用");
    out
}

fn expand(key: &[u8], info: &[u8], out: &mut [u8]) {
    Hkdf::<Sha256>::new(None, key)
        .expand(info, out)
        .expect("HKDF 输出长度合法");
}

pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC 接受任意长度的密钥");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// 不随内容提前返回的比较（验 MAC 用）。
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 从组密钥派生出来的各把钥匙。
#[derive(Clone)]
pub struct GroupKeys {
    enc: [u8; KEY_LEN],
    beacon: [u8; KEY_LEN],
    /// 中转服务上这组设备的信道名（32 位十六进制）。从组密钥派生，中转服务拿它反推不出密钥。
    pub channel: String,
}

impl GroupKeys {
    pub fn derive(group_key: &[u8; KEY_LEN]) -> Self {
        let mut enc = [0u8; KEY_LEN];
        let mut beacon = [0u8; KEY_LEN];
        let mut channel = [0u8; 16];
        expand(group_key, b"wowtype-sync v1 enc", &mut enc);
        expand(group_key, b"wowtype-sync v1 beacon", &mut beacon);
        expand(group_key, b"wowtype-sync v1 relay channel", &mut channel);
        Self {
            enc,
            beacon,
            channel: hex_encode(&channel),
        }
    }

    /// 局域网广播的签名：只有同一组的设备认得出彼此。
    pub fn beacon_mac(&self, data: &[u8]) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&hmac(&self.beacon, &[data])[..16]);
        out
    }

    /// 把一段内容封成信封。
    pub fn seal(&self, sender: &DeviceId, ts_ms: u64, plaintext: &[u8]) -> Vec<u8> {
        let nonce: [u8; NONCE_LEN] = random();
        let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + plaintext.len() + 16);
        out.extend_from_slice(MAGIC);
        out.push(KIND_CLIP);
        out.extend_from_slice(sender);
        out.extend_from_slice(&ts_ms.to_be_bytes());
        let ciphertext = XChaCha20Poly1305::new(Key::from_slice(&self.enc))
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &out[..HEADER_LEN],
                },
            )
            .expect("加密不会失败");
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        out
    }

    /// 拆信封。不是这一组的、被改过的都拆不开。
    pub fn open(&self, envelope: &[u8]) -> Option<Opened> {
        if envelope.len() < HEADER_LEN + NONCE_LEN + 16
            || &envelope[..4] != MAGIC
            || envelope[4] != KIND_CLIP
        {
            return None;
        }
        let (header, rest) = envelope.split_at(HEADER_LEN);
        let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
        let plaintext = XChaCha20Poly1305::new(Key::from_slice(&self.enc))
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: header,
                },
            )
            .ok()?;
        let mut sender = [0u8; DEVICE_ID_LEN];
        sender.copy_from_slice(&header[5..5 + DEVICE_ID_LEN]);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&header[5 + DEVICE_ID_LEN..]);
        let mut id = [0u8; NONCE_LEN];
        id.copy_from_slice(nonce);
        Some(Opened {
            sender,
            ts_ms: u64::from_be_bytes(ts),
            id,
            plaintext,
        })
    }
}

/// 不拆信封就能看的发送设备（收到自己发的那份时直接丢掉）。
pub fn envelope_sender(envelope: &[u8]) -> Option<DeviceId> {
    if envelope.len() < HEADER_LEN || &envelope[..4] != MAGIC {
        return None;
    }
    let mut sender = [0u8; DEVICE_ID_LEN];
    sender.copy_from_slice(&envelope[5..5 + DEVICE_ID_LEN]);
    Some(sender)
}

pub struct Opened {
    pub sender: DeviceId,
    pub ts_ms: u64,
    /// 每个信封一个随机数，也当它的编号（同一封信从局域网和中转各来一次，只收一次）。
    pub id: [u8; NONCE_LEN],
    pub plaintext: Vec<u8>,
}

/// 用一次性的密钥加密一小段（配对时传组密钥）。
pub fn seal_with(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let nonce: [u8; NONCE_LEN] = random();
    let mut out = nonce.to_vec();
    out.extend(
        XChaCha20Poly1305::new(Key::from_slice(key))
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("加密不会失败"),
    );
    out
}

pub fn open_with(key: &[u8; KEY_LEN], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < NONCE_LEN + 16 {
        return None;
    }
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    XChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .ok()
}

/// 从配对的会话密钥派生一把子密钥。
pub fn derive(key: &[u8], info: &[u8]) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    expand(key, info, &mut out);
    out
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if text.len() % 2 != 0 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    text.chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 Base64（带 `=`）。中转服务的接口和状态文件用。
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=').as_bytes();
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let value = B64.iter().position(|b| b == c)? as u32;
            n |= value << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip_and_tamper() {
        let keys = GroupKeys::derive(&[7u8; 32]);
        let sender = [1u8; 8];
        let sealed = keys.seal(&sender, 1234, "你好 hello".as_bytes());
        assert_eq!(envelope_sender(&sealed), Some(sender));
        let opened = keys.open(&sealed).unwrap();
        assert_eq!(opened.sender, sender);
        assert_eq!(opened.ts_ms, 1234);
        assert_eq!(opened.plaintext, "你好 hello".as_bytes());
        assert!(!sealed
            .windows(5)
            .any(|w| w == "你好".as_bytes().get(..5).unwrap()));

        // 每次的随机数不一样，密文也不一样
        let again = keys.seal(&sender, 1234, "你好 hello".as_bytes());
        assert_ne!(sealed, again);
        assert_ne!(opened.id, keys.open(&again).unwrap().id);

        // 改了头（发送设备、时间）、改了密文、换了组，都拆不开
        for index in [5, 14, HEADER_LEN + 3, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[index] ^= 1;
            assert!(keys.open(&bad).is_none(), "改了第 {index} 字节还能拆开");
        }
        assert!(GroupKeys::derive(&[8u8; 32]).open(&sealed).is_none());
        assert!(keys.open(&sealed[..20]).is_none());
        assert!(keys.open(b"").is_none());
    }

    #[test]
    fn derived_keys_differ_and_are_stable() {
        let a = GroupKeys::derive(&[1u8; 32]);
        let b = GroupKeys::derive(&[2u8; 32]);
        assert_ne!(a.channel, b.channel);
        assert_eq!(a.channel, GroupKeys::derive(&[1u8; 32]).channel);
        assert_eq!(a.channel.len(), 32);
        assert_ne!(a.enc, a.beacon);
        assert_eq!(a.beacon_mac(b"x"), a.beacon_mac(b"x"));
        assert_ne!(a.beacon_mac(b"x"), b.beacon_mac(b"x"));
        // 信道名不是密钥的一部分
        assert!(!hex_encode(&[1u8; 32]).contains(&a.channel));
    }

    #[test]
    fn base64_and_hex() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(raw), text);
            assert_eq!(b64_decode(text).unwrap(), raw);
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(b64_decode(&b64_encode(&all)).unwrap(), all);
        assert!(b64_decode("a").is_none() && b64_decode("ab!d").is_none());
        assert_eq!(hex_encode(&[0, 15, 255]), "000fff");
        assert_eq!(hex_decode("000fff").unwrap(), vec![0, 15, 255]);
        assert!(hex_decode("0").is_none() && hex_decode("zz").is_none());
    }

    #[test]
    fn one_time_seal() {
        let key = [3u8; 32];
        let sealed = seal_with(&key, b"aad", b"secret");
        assert_eq!(open_with(&key, b"aad", &sealed).unwrap(), b"secret");
        assert!(open_with(&key, b"other", &sealed).is_none());
        assert!(open_with(&[4u8; 32], b"aad", &sealed).is_none());
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
    }
}
