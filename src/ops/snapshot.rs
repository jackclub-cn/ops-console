//! 快照轮转：删旧建新，控制每台服务器保留的快照数量。

use crate::cloud::{CloudProvider, SnapshotStatus};
use anyhow::{anyhow, Result};
use chrono::Utc;
use std::time::Duration;

/// 轮转结果摘要
#[derive(Debug, Default)]
pub struct RotateSummary {
    pub server_name: String,
    pub deleted: Vec<String>,
    pub created_id: Option<String>,
    pub created_name: Option<String>,
    pub remaining: usize,
}

impl RotateSummary {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("=== 快照轮转完成: {} ===\n", self.server_name));
        if self.deleted.is_empty() {
            out.push_str("  删除: 无（无需清理）\n");
        } else {
            out.push_str(&format!("  删除: {}\n", self.deleted.join(", ")));
        }
        if let (Some(id), Some(name)) = (&self.created_id, &self.created_name) {
            out.push_str(&format!("  新建: {id} ({name})\n"));
        }
        out.push_str(&format!("  当前快照总数: {}\n", self.remaining));
        out
    }
}

/// 生成快照名：`{prefix}-{YYYYMMDD-HHMMSS}`（北京时间，与阿里云控制台显示一致）
pub fn default_snapshot_name(prefix: &str) -> String {
    let ts = (Utc::now() + chrono::Duration::hours(8)).format("%Y%m%d-%H%M%S");
    format!("{prefix}-{ts}")
}

/// 快照轮转。
///
/// 策略：保留最新 `keep` 份可用的旧快照，删除更旧的（正在创建中的不动），
/// 然后创建一份新快照，并等待其变为 Available。
///
/// 注意阿里云轻量限制：单台最多 3 个快照。keep=2 时轮转窗口 = 2 旧 + 1 新建中。
pub async fn rotate<P: CloudProvider + ?Sized>(
    provider: &P,
    server_id: &str,
    keep: usize,
    wait_timeout: Duration,
) -> Result<RotateSummary> {
    if keep == 0 {
        return Err(anyhow!("keep 必须 >= 1"));
    }

    // 服务器名称（用于命名）
    let server_name = provider
        .list_servers()
        .await?
        .into_iter()
        .find(|s| s.id == server_id)
        .map(|s| s.name)
        .unwrap_or_else(|| server_id.to_string());

    let mut snapshots = provider.list_snapshots(server_id).await?;
    snapshots.sort_by(|a, b| a.created_at.cmp(&b.created_at)); // 旧 -> 新

    let creating: Vec<&crate::cloud::Snapshot> =
        snapshots.iter().filter(|s| s.status == SnapshotStatus::Creating).collect();
    let failed: Vec<&crate::cloud::Snapshot> =
        snapshots.iter().filter(|s| s.status == SnapshotStatus::Failed).collect();
    let mut available: Vec<crate::cloud::Snapshot> = snapshots
        .iter()
        .filter(|s| s.status == SnapshotStatus::Available)
        .cloned()
        .collect();
    // 最新的 keep-1 个保留（因为马上会新建 1 个顶到 keep）
    let keep_old = keep.saturating_sub(1);

    let mut summary = RotateSummary {
        server_name,
        ..Default::default()
    };

    // 删掉多余的旧快照（从最旧开始）
    while available.len() > keep_old {
        let victim = available.remove(0);
        match provider.delete_snapshot(&victim.id).await {
            Ok(()) => {
                tracing::info!("已删除旧快照 {} ({})", victim.id, victim.name);
                summary.deleted.push(victim.name);
            }
            Err(e) => {
                tracing::warn!("删除快照 {} 失败: {e}", victim.id);
            }
        }
    }

    if !creating.is_empty() {
        tracing::warn!(
            "检测到 {} 个创建中的快照，等待其完成再新建（避免超过单台上限）",
            creating.len()
        );
        let deadline = tokio::time::Instant::now() + wait_timeout;
        loop {
            let snaps = provider.list_snapshots(server_id).await?;
            let still_creating: Vec<_> = snaps
                .iter()
                .filter(|s| s.status == SnapshotStatus::Creating)
                .collect();
            if still_creating.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!("等待 {} 个创建中的快照完成超时", still_creating.len()));
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    // 删除 Failed 快照（占名额且无用）
    for f in failed {
        match provider.delete_snapshot(&f.id).await {
            Ok(()) => {
                tracing::info!("已删除失败快照 {} ({})", f.id, f.name);
                summary.deleted.push(f.name.clone());
            }
            Err(e) => tracing::warn!("删除失败快照 {} 出错: {e}", f.id),
        }
    }

    // 新建快照
    let name = default_snapshot_name(&summary.server_name);
    let snap_id = provider.create_snapshot(server_id, &name).await?;
    tracing::info!("已创建快照 {snap_id} ({name})");
    summary.created_id = Some(snap_id.clone());
    summary.created_name = Some(name);

    // 等待就绪
    provider.wait_snapshot_ready(server_id, &snap_id, wait_timeout).await?;

    // 统计剩余
    let after = provider.list_snapshots(server_id).await?;
    summary.remaining = after.len();

    Ok(summary)
}
