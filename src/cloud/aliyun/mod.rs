//! 阿里云 provider 实现。

pub mod sign;
pub mod swas;

use crate::cloud::{CloudProvider, Server, Snapshot, SnapshotStatus};
use anyhow::{anyhow, Result};
use std::time::Duration;

pub struct AliyunProvider {
    region: String,
    swas: swas::SwasClient,
}

impl AliyunProvider {
    pub fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Self {
        Self {
            region: region.to_string(),
            swas: swas::SwasClient::new(access_key_id, access_key_secret, region),
        }
    }
}

fn map_status(s: &str) -> SnapshotStatus {
    match s {
        // 阿里云轻量实际状态值：progressing（创建中）/ accomplished（已完成）
        "Creating" | "progressing" => SnapshotStatus::Creating,
        "accomplished" | "Available" => SnapshotStatus::Available,
        "Failed" => SnapshotStatus::Failed,
        _ => SnapshotStatus::Unknown,
    }
}

#[async_trait::async_trait]
impl CloudProvider for AliyunProvider {
    async fn list_servers(&self) -> Result<Vec<Server>> {
        let instances = self.swas.list_instances().await?;
        Ok(instances
            .into_iter()
            .map(|i| {
                let id = i.instance_id.clone();
                // ExpiredTime 为 ISO8601（Z 结尾），解析失败视为无到期时间
                let expired_at = if i.expired_time.is_empty() {
                    None
                } else {
                    chrono::DateTime::parse_from_rfc3339(&i.expired_time)
                        .ok()
                        .map(|t| t.with_timezone(&chrono::Utc))
                };
                Server {
                    id,
                    name: if i.instance_name.is_empty() {
                        i.instance_id
                    } else {
                        i.instance_name
                    },
                    region: self.region.clone(),
                    status: i.status,
                    expired_at,
                }
            })
            .collect())
    }

    async fn list_snapshots(&self, server_id: &str) -> Result<Vec<Snapshot>> {
        let snaps = self.swas.list_snapshots(server_id).await?;
        let mut out: Vec<Snapshot> = snaps
            .into_iter()
            .map(|s| {
                let id = s.snapshot_id.clone();
                Snapshot {
                    id,
                    name: if s.snapshot_name.is_empty() {
                        s.snapshot_id
                    } else {
                        s.snapshot_name
                    },
                    status: map_status(&s.status),
                    created_at: if s.creation_time.is_empty() {
                        None
                    } else {
                        Some(s.creation_time)
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    async fn create_snapshot(&self, server_id: &str, name: &str) -> Result<String> {
        self.swas.create_snapshot(server_id, name).await
    }

    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        self.swas.delete_snapshot(snapshot_id).await
    }

    async fn wait_snapshot_ready(
        &self,
        server_id: &str,
        snapshot_id: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "等待快照 {snapshot_id} 就绪超时（{} 分钟）",
                    timeout.as_secs() / 60
                ));
            }
            let snaps = self.swas.list_snapshots(server_id).await?;
            match snaps.iter().find(|s| s.snapshot_id == snapshot_id) {
                Some(s) => {
                    let cur = format!("{} ({})", s.status, s.progress);
                    match map_status(&s.status) {
                        SnapshotStatus::Available => {
                            tracing::info!("快照 {snapshot_id} 已就绪");
                            return Ok(());
                        }
                        SnapshotStatus::Failed => {
                            return Err(anyhow!("快照 {snapshot_id} 创建失败（Failed）"));
                        }
                        _ => tracing::info!("等待快照 {snapshot_id}... 当前: {cur}"),
                    }
                }
                None => {
                    tracing::info!("等待快照 {snapshot_id}... 未在列表中");
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}
