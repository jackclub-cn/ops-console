//! 阿里云轻量应用服务器（SWAS / swas-openapi）API 封装。
//!
//! 产品文档：https://help.aliyun.com/zh/simple-application-server/
//! endpoint: https://swas.{region}.aliyuncs.com
//! API Version: 2020-06-01

use super::rpc::RpcClient;
use anyhow::{anyhow, Result};
use serde::Deserialize;

const SWAS_API_VERSION: &str = "2020-06-01";

#[derive(Debug, Clone)]
pub struct SwasClient {
    rpc: RpcClient,
}

// ---------- 请求/响应模型 ----------

#[derive(Debug, Deserialize)]
pub struct ListInstancesResponse {
    #[serde(rename = "Instances", default)]
    pub instances: Vec<SwasInstance>,
    #[serde(rename = "TotalCount", default)]
    pub total_count: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SwasInstance {
    #[serde(rename = "InstanceId")]
    pub instance_id: String,
    #[serde(rename = "InstanceName", default)]
    pub instance_name: String,
    #[serde(rename = "Status", default)]
    pub status: String,
    /// 到期时间（ISO8601 UTC，如 2026-09-01T16:00:00Z）；按量付费实例可能为空
    #[serde(rename = "ExpiredTime", default)]
    pub expired_time: String,
}

#[derive(Debug, Deserialize)]
pub struct ListDisksResponse {
    #[serde(rename = "Disks", default)]
    pub disks: Vec<SwasDisk>,
    /// 轻量实例磁盘最多 2 块（系统盘+数据盘），无需分页；保留字段供校验
    #[allow(dead_code)]
    #[serde(rename = "TotalCount", default)]
    pub total_count: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SwasDisk {
    #[serde(rename = "DiskId")]
    pub disk_id: String,
    /// System | Data
    #[serde(rename = "DiskType", default)]
    pub disk_type: String,
    #[serde(rename = "DiskName", default)]
    pub disk_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ListSnapshotsResponse {
    #[serde(rename = "Snapshots", default)]
    pub snapshots: Vec<SwasSnapshot>,
    #[serde(rename = "TotalCount", default)]
    pub total_count: i32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SwasSnapshot {
    #[serde(rename = "SnapshotId")]
    pub snapshot_id: String,
    #[serde(rename = "SnapshotName", default)]
    pub snapshot_name: String,
    /// Creating | Available | Failed
    #[serde(rename = "Status", default)]
    pub status: String,
    /// 创建进度（百分比字符串）
    #[serde(rename = "Progress", default)]
    pub progress: String,
    #[serde(rename = "CreationTime", default)]
    pub creation_time: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSnapshotResponse {
    #[serde(rename = "SnapshotId")]
    pub snapshot_id: String,
}

// ---------- 客户端 ----------

impl SwasClient {
    pub fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Self {
        Self {
            rpc: RpcClient::new(access_key_id, access_key_secret, region, "swas"),
        }
    }

    pub async fn list_instances(&self) -> Result<Vec<SwasInstance>> {
        self.rpc
            .paginate("ListInstances", SWAS_API_VERSION, &[], |resp: ListInstancesResponse| {
                (resp.instances, resp.total_count)
            })
            .await
    }

    pub async fn list_snapshots(&self, instance_id: &str) -> Result<Vec<SwasSnapshot>> {
        self.rpc
            .paginate(
                "ListSnapshots",
                SWAS_API_VERSION,
                &[("InstanceId", instance_id)],
                |resp: ListSnapshotsResponse| (resp.snapshots, resp.total_count),
            )
            .await
    }

    pub async fn list_disks(&self, instance_id: &str) -> Result<Vec<SwasDisk>> {
        let resp: ListDisksResponse = self
            .rpc
            .call("ListDisks", SWAS_API_VERSION, &[("InstanceId", instance_id)])
            .await?;
        Ok(resp.disks)
    }

    /// 创建快照：轻量快照是磁盘级，需先解析系统盘 DiskId
    pub async fn create_snapshot(&self, instance_id: &str, name: &str) -> Result<String> {
        let disks = self.list_disks(instance_id).await?;
        let disk = disks
            .iter()
            .find(|d| d.disk_type == "System")
            .or_else(|| disks.first())
            .ok_or_else(|| anyhow!("实例 {instance_id} 没有可用的磁盘"))?;
        tracing::info!(
            "实例 {instance_id} 使用磁盘 {} ({}, {}) 创建快照",
            disk.disk_id,
            disk.disk_type,
            disk.disk_name
        );
        let resp: CreateSnapshotResponse = self
            .rpc
            .call(
                "CreateSnapshot",
                SWAS_API_VERSION,
                &[("DiskId", disk.disk_id.as_str()), ("SnapshotName", name)],
            )
            .await?;
        Ok(resp.snapshot_id)
    }

    pub async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        // 删除成功返回 RequestId，无业务字段
        let _: serde_json::Value =
            self.rpc
                .call("DeleteSnapshot", SWAS_API_VERSION, &[("SnapshotId", snapshot_id)])
                .await?;
        Ok(())
    }
}
