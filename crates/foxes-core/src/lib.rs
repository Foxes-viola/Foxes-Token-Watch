//! Foxes Token Watch 内核：读取本机各 AI agent 的会话记录，统计 token 用量与实时状态。
//!
//! 只读；只取用量数字、时间和状态，不保留对话正文。
//!
//! 只连一个地址：百事通插件在本机回环地址上开的转发口（它的对话不在本机的文件里，见 `sources::baishitong`），
//! 只发 GET。别的网络地址一概不连，也不读任何凭据文件。

pub mod aggregate;
pub mod estimate;
pub mod history;
pub mod intake;
pub mod lenient;
pub mod live;
pub mod model;
pub mod proto;
pub mod sources;
pub mod store;
pub mod timeutil;
pub mod zstdframes;

pub use aggregate::{analyze, Analysis, SessionView, Snapshot};
pub use model::{Agent, Phase, Usage};
pub use sources::Roots;
pub use store::Store;
