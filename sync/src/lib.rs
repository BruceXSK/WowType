//! WowType 跨设备剪贴板。
//!
//! 一台电脑上复制的文字，在另一台装了 WowType 的电脑上可以直接粘贴。没有账号，也没有 WowType 的服务器：
//!
//! - **配对**：一台设备显示 6 位配对码，另一台输入。配对码经 SPAKE2 换成会话密钥，再把 32 字节的
//!   组密钥传过去；之后这组设备都拿这把组密钥。
//! - **加密**：剪贴板内容用组密钥派生的密钥端到端加密（XChaCha20-Poly1305），见 [`crypto`]。
//! - **传输**两条路，按需打开：局域网直连（[`lan`]，自己广播、自己发现），和用户自建的中转服务
//!   （[`relay`]，Cloudflare Worker，只看得到密文）。两条路传的是同一种信封。
//!
//! 输入法这边只用 [`Service`]：告诉它本机剪贴板现在是什么（[`Service::observe`]），定时取收到的
//! （[`Service::poll`]）。配对由设置界面做（[`cli`]、[`pairing`]）。

pub mod cli;
pub mod crypto;
pub mod store;
pub mod pairing;
pub mod lan;
pub mod relay;
pub mod service;

pub use service::{Config, Received, RelayStatus, Service, Status};
