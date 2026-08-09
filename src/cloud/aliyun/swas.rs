//! 阿里云轻量应用服务器（SWAS / swas-openapi）API 封装。
//!
//! 产品文档：https://help.aliyun.com/zh/simple-application-server/
//! endpoint: https://swas.{region}.aliyuncs.com
//! API Version: 2020-06-01

use super::sign::sign_params;
use anyhow::{anyhow, Result};
use serde::Deserialize;

const SWAS_API_VERSION: &str = "2020-06-01";

#[derive(Debug, Clone)]
pub struct SwasClient {
    access_key_id: String,
    access_key_secret: String,
    region: String,
    http: reqwest::Client,
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
            access_key_id: access_key_id.to_string(),
            access_key_secret: access_key_secret.to_string(),
            region: region.to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("构建 HTTP client 失败"),
        }
    }

    /// 通用 RPC 调用：签名 + GET + 统一错误处理
    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        action: &str,
        extra: &[(&str, &str)],
    ) -> Result<T> {
        let params = sign_params(
            &self.access_key_id,
            &self.access_key_secret,
            action,
            SWAS_API_VERSION,
            &self.region,
            extra,
        )?;

        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("https://swas.{}.aliyuncs.com/?{}", self.region, query);

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            return Err(anyhow!(
                "SWAS API HTTP {} ({}): {}",
                status.as_u16(),
                action,
                truncate(&text, 500)
            ));
        }

        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("响应解析失败 ({action}): {e} => {}", truncate(&text, 300)))?;

        // 阿里云 RPC 风格：HTTP 200 也可能带业务错误码
        if let Some(code) = value.get("Code").and_then(|c| c.as_str()) {
            if !code.is_empty() && code != "Success" {
                let msg = value
                    .get("Message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                return Err(anyhow!("SWAS 业务错误 {code}: {msg}"));
            }
        }

        serde_json::from_value(value).map_err(|e| anyhow!("响应反序列化失败 ({action}): {e}"))
    }

    pub async fn list_instances(&self) -> Result<Vec<SwasInstance>> {
        self.paginate("ListInstances", &[], |resp: ListInstancesResponse| {
            (resp.instances, resp.total_count)
        })
        .await
    }

    pub async fn list_snapshots(&self, instance_id: &str) -> Result<Vec<SwasSnapshot>> {
        self.paginate("ListSnapshots", &[("InstanceId", instance_id)], |resp: ListSnapshotsResponse| {
            (resp.snapshots, resp.total_count)
        })
        .await
    }

    /// 通用分页拉取：SWAS 默认每页 10 条，必须翻页才能取全（否则快照 >10 会漏删）。
    /// `extract` 从单页响应中取出 `(数据, TotalCount)`，循环取到 TotalCount 为止。
    async fn paginate<T, E>(
        &self,
        action: &str,
        extra: &[(&str, &str)],
        extract: impl Fn(E) -> (Vec<T>, i32),
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
        E: serde::de::DeserializeOwned,
    {
        const PAGE_SIZE: i32 = 100; // SWAS 上限
        let mut page = 1;
        let mut out = Vec::new();
        loop {
            let page_str = page.to_string();
            let size_str = PAGE_SIZE.to_string();
            let params: Vec<(&str, &str)> = extra
                .iter()
                .copied()
                .chain([
                    ("PageNumber", page_str.as_str()),
                    ("PageSize", size_str.as_str()),
                ])
                .collect();
            let resp: E = self.call(action, &params).await?;
            let (items, total) = extract(resp);
            out.extend(items);
            let fetched = out.len() as i32;
            if fetched >= total {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    pub async fn list_disks(&self, instance_id: &str) -> Result<Vec<SwasDisk>> {
        let resp: ListDisksResponse = self.call("ListDisks", &[("InstanceId", instance_id)]).await?;
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
            .call(
                "CreateSnapshot",
                &[("DiskId", disk.disk_id.as_str()), ("SnapshotName", name)],
            )
            .await?;
        Ok(resp.snapshot_id)
    }

    pub async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        // 删除成功返回 RequestId，无业务字段
        let _: serde_json::Value = self.call("DeleteSnapshot", &[("SnapshotId", snapshot_id)]).await?;
        Ok(())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}...")
    }
}
