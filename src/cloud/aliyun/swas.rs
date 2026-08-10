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
    /// 磁盘容量（GB）
    #[serde(rename = "Size", default)]
    pub size: i32,
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

#[derive(Debug, Deserialize)]
pub struct DescribeMonitorDataResponse {
    /// Datapoints 是 JSON 编码的字符串数组（元素含 timestamp 与数值字段）
    #[serde(rename = "Datapoints", default)]
    pub datapoints: String,
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

    /// 查询实例磁盘已用空间（bytes）。
    /// 近 10 分钟窗口内无监控数据（如未装云监控插件）返回 None。
    pub async fn disk_usage_used(&self, instance_id: &str) -> Result<Option<u64>> {
        let now = chrono::Utc::now();
        let start = now - chrono::Duration::minutes(10);
        let start_s = start.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let end_s = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let resp: DescribeMonitorDataResponse = self
            .rpc
            .call(
                "DescribeMonitorData",
                SWAS_API_VERSION,
                &[
                    ("InstanceId", instance_id),
                    ("MetricName", "DISKUSAGE_USED"),
                    ("Period", "300"),
                    ("StartTime", start_s.as_str()),
                    ("EndTime", end_s.as_str()),
                ],
            )
            .await?;
        Ok(parse_latest_usage(&resp.datapoints))
    }
}

/// 解析 DescribeMonitorData 的 Datapoints（JSON 数组字符串），返回磁盘已用空间（bytes）。
/// 元素数值字段兼容 `Value`/`value`/`Average`；时间戳兼容 `timestamp`/`Timestamp`。
/// 策略：取最新时间戳，同一时刻可能返回多条序列（如多个分区的已用空间），取其中最大值。
/// 空/非法/无数值字段 → None。
fn parse_latest_usage(datapoints: &str) -> Option<u64> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(datapoints).ok()?;
    let latest_ts = arr
        .iter()
        .filter_map(|p| {
            p.get("timestamp")
                .or_else(|| p.get("Timestamp"))
                .and_then(|v| v.as_i64())
        })
        .max()?;
    arr.iter()
        .filter(|p| {
            p.get("timestamp")
                .or_else(|| p.get("Timestamp"))
                .and_then(|v| v.as_i64())
                == Some(latest_ts)
        })
        .filter_map(|p| {
            p.get("Value")
                .or_else(|| p.get("value"))
                .or_else(|| p.get("Average"))
                .and_then(|v| v.as_f64())
                .map(|f| f as u64)
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_size_field() {
        let j = r#"{"DiskId":"d-xxx","DiskType":"System","DiskName":"系统盘","Size":50}"#;
        let d: SwasDisk = serde_json::from_str(j).unwrap();
        assert_eq!(d.size, 50);
    }

    #[test]
    fn test_parse_latest_usage() {
        // 取时间戳最新的点
        let s = r#"[{"timestamp": 1699219200, "Value": 1000}, {"timestamp": 1699219500, "Value": 2000}]"#;
        assert_eq!(parse_latest_usage(s), Some(2000));

        // 同一时间戳多条序列（真实响应：同一时刻返回多个分区的已用空间）→ 取最大值
        let s = r#"[{"timestamp": 1699219500, "Value": 6402048}, {"timestamp": 1699219500, "Value": 5707819008}]"#;
        assert_eq!(parse_latest_usage(s), Some(5707819008));

        // 旧时间戳数值更大也不取（只看最新时刻）
        let s = r#"[{"timestamp": 1699219200, "Value": 999999999}, {"timestamp": 1699219500, "Value": 100}]"#;
        assert_eq!(parse_latest_usage(s), Some(100));

        // 小写 value 兼容；缺 timestamp 按 0 处理
        let s = r#"[{"timestamp": 1699219500, "value": 3000}]"#;
        assert_eq!(parse_latest_usage(s), Some(3000));

        // Average 兜底（字段名变体）
        let s = r#"[{"timestamp": 1, "Average": 4096.0}]"#;
        assert_eq!(parse_latest_usage(s), Some(4096));

        // 大写 Timestamp 兼容
        let s = r#"[{"Timestamp": 1699219500, "Value": 500}]"#;
        assert_eq!(parse_latest_usage(s), Some(500));

        // 空数组 / 空串 / 非 JSON / 无数值字段 → None
        assert_eq!(parse_latest_usage("[]"), None);
        assert_eq!(parse_latest_usage(""), None);
        assert_eq!(parse_latest_usage("not json"), None);
        let s = r#"[{"timestamp": 1}]"#;
        assert_eq!(parse_latest_usage(s), None);
    }
}
