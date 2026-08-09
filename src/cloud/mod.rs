//! 服务商抽象层。
//!
//! 新服务商接入 = 实现 [`CloudProvider`] trait + 一个 API 模块。
//! 例如：腾讯云 -> `cloud::tencent`，AWS -> `cloud::aws`，RackNerd -> `cloud::racknerd`。

pub mod aliyun;

use serde::{Deserialize, Serialize};

/// 统一的服务器/实例抽象（跨服务商）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub region: String,
    /// 服务商原始状态字符串（如 Running / stopped）
    pub status: String,
}

/// 快照状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotStatus {
    Creating,
    Available,
    Failed,
    Unknown,
}

/// 统一的快照抽象
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub status: SnapshotStatus,
    /// 服务商原始创建时间字符串（排序用）
    pub created_at: Option<String>,
}

/// 服务商抽象：运维系统的扩展点。
///
/// 所有运维操作（快照轮转、未来的备份/监控/账单）都只依赖这个 trait，
/// 具体服务商的差异被隔离在实现内部。
#[async_trait::async_trait]
pub trait CloudProvider: Send + Sync {
    /// 列出该服务商下的所有服务器
    async fn list_servers(&self) -> anyhow::Result<Vec<Server>>;

    /// 列出指定服务器的全部快照（按创建时间升序，旧在前）
    async fn list_snapshots(&self, server_id: &str) -> anyhow::Result<Vec<Snapshot>>;

    /// 创建快照，返回快照 ID
    async fn create_snapshot(&self, server_id: &str, name: &str) -> anyhow::Result<String>;

    /// 删除快照
    async fn delete_snapshot(&self, snapshot_id: &str) -> anyhow::Result<()>;

    /// 等待快照变为可用，超时返回错误
    async fn wait_snapshot_ready(
        &self,
        server_id: &str,
        snapshot_id: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()>;
}
