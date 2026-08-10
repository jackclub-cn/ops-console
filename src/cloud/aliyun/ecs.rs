//! 阿里云云服务器 ECS（ecs / ecs-openapi）API 封装。
//!
//! 产品文档：https://help.aliyun.com/zh/ecs/
//! endpoint: https://ecs.{region}.aliyuncs.com
//! API Version: 2014-05-26

use super::rpc::{parse_expired_time, RpcClient};
use crate::cloud::Server;
use anyhow::Result;
use serde::Deserialize;

const ECS_API_VERSION: &str = "2014-05-26";

#[derive(Debug, Clone)]
pub struct EcsClient {
    rpc: RpcClient,
}

// ---------- 请求/响应模型 ----------
// ECS 响应是"嵌套数组"结构：{ "Instances": { "Instance": [...] }, "TotalCount": N }
// 不同 API 的数组键名不同（Instance / Disk / AutoSnapshotPolicy），在各方法内定义包装结构。

#[derive(Debug, Deserialize)]
pub struct EcsInstance {
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
pub struct EcsDisk {
    #[serde(rename = "DiskId")]
    pub disk_id: String,
    /// system | data
    #[serde(rename = "DiskType", default)]
    pub disk_type: String,
    /// 云盘采用的自动快照策略 ID；未绑定为空
    /// （多策略时只返回其中一条，判断"是否开启"足够）
    #[serde(rename = "AutoSnapshotPolicyId", default)]
    pub auto_snapshot_policy_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EcsAutoSnapshotPolicy {
    #[serde(rename = "AutoSnapshotPolicyId")]
    pub policy_id: String,
    #[serde(rename = "AutoSnapshotPolicyName", default)]
    pub name: String,
    /// 触发时间点（JSON 数组字符串，如 ["0","1"]）
    #[serde(rename = "TimePoints", default)]
    pub time_points: String,
    /// 重复周期（JSON 数组字符串，1-7 表示周一至周日）
    #[serde(rename = "RepeatWeekdays", default)]
    pub repeat_weekdays: String,
    #[serde(rename = "RetentionDays", default)]
    pub retention_days: i32,
}

impl EcsAutoSnapshotPolicy {
    /// 策略摘要：`名称（触发 00:00,02:00，周一,周三，保留 7 天）`
    pub fn summary(&self) -> String {
        let mut s = self.name.clone();
        let mut parts = Vec::new();
        if !self.time_points.is_empty() {
            parts.push(format!("触发 {}", fmt_time_points(&self.time_points)));
        }
        if !self.repeat_weekdays.is_empty() {
            parts.push(fmt_weekdays(&self.repeat_weekdays));
        }
        if self.retention_days > 0 {
            parts.push(format!("保留 {} 天", self.retention_days));
        }
        if !parts.is_empty() {
            s.push_str(&format!("（{}）", parts.join("，")));
        }
        s
    }
}

/// JSON 字符串数组（小时）→ "00:00,02:00"；解析失败返回原串
fn fmt_time_points(s: &str) -> String {
    serde_json::from_str::<Vec<String>>(s)
        .map(|v| {
            v.iter()
                .map(|h| format!("{:0>2}:00", h))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|_| s.to_string())
}

/// JSON 字符串数组（1=周一 ... 7=周日）→ "周一,周三"；解析失败返回原串
fn fmt_weekdays(s: &str) -> String {
    const NAMES: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    serde_json::from_str::<Vec<String>>(s)
        .map(|v| {
            v.iter()
                .map(|d| {
                    d.parse::<usize>()
                        .ok()
                        .and_then(|i| i.checked_sub(1).and_then(|i| NAMES.get(i)))
                        .copied()
                        .unwrap_or(d.as_str())
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_time_points() {
        assert_eq!(fmt_time_points("[\"0\",\"2\"]"), "00:00,02:00");
        assert_eq!(fmt_time_points("[\"9\"]"), "09:00");
        assert_eq!(fmt_time_points("bad"), "bad");
    }

    #[test]
    fn test_fmt_weekdays() {
        assert_eq!(fmt_weekdays("[\"1\",\"5\"]"), "周一,周五");
        assert_eq!(fmt_weekdays("[\"7\"]"), "周日");
        assert_eq!(fmt_weekdays("[\"8\"]"), "8");
    }

    #[test]
    fn test_policy_summary() {
        let p = EcsAutoSnapshotPolicy {
            policy_id: "sp-1".into(),
            name: "每日快照".into(),
            time_points: "[\"0\"]".into(),
            repeat_weekdays: "[\"1\",\"2\",\"3\",\"4\",\"5\"]".into(),
            retention_days: 7,
        };
        assert_eq!(
            p.summary(),
            "每日快照（触发 00:00，周一,周二,周三,周四,周五，保留 7 天）"
        );
    }
}

// ---------- 客户端 ----------

impl EcsClient {
    pub fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Self {
        Self {
            rpc: RpcClient::new(access_key_id, access_key_secret, region, "ecs"),
        }
    }

    pub async fn list_instances(&self) -> Result<Vec<EcsInstance>> {
        #[derive(Deserialize, Default)]
        struct InstanceList {
            #[serde(rename = "Instance", default)]
            items: Vec<EcsInstance>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "Instances", default)]
            instances: InstanceList,
            #[serde(rename = "TotalCount", default)]
            total_count: i32,
        }
        self.rpc
            .paginate("DescribeInstances", ECS_API_VERSION, &[], |resp: Resp| {
                (resp.instances.items, resp.total_count)
            })
            .await
    }

    /// 列出实例的云盘（DescribeDisks 支持 InstanceId 参数，无需 Filter）
    pub async fn list_disks(&self, instance_id: &str) -> Result<Vec<EcsDisk>> {
        #[derive(Deserialize, Default)]
        struct DiskList {
            #[serde(rename = "Disk", default)]
            items: Vec<EcsDisk>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "Disks", default)]
            disks: DiskList,
            #[serde(rename = "TotalCount", default)]
            total_count: i32,
        }
        self.rpc
            .paginate(
                "DescribeDisks",
                ECS_API_VERSION,
                &[("InstanceId", instance_id)],
                |resp: Resp| (resp.disks.items, resp.total_count),
            )
            .await
    }

    /// 列出地域内全部自动快照策略
    pub async fn list_auto_snapshot_policies(&self) -> Result<Vec<EcsAutoSnapshotPolicy>> {
        #[derive(Deserialize, Default)]
        struct PolicyList {
            #[serde(rename = "AutoSnapshotPolicy", default)]
            items: Vec<EcsAutoSnapshotPolicy>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "AutoSnapshotPolicies", default)]
            policies: PolicyList,
            #[serde(rename = "TotalCount", default)]
            total_count: i32,
        }
        self.rpc
            .paginate("DescribeAutoSnapshotPolicyEx", ECS_API_VERSION, &[], |resp: Resp| {
                (resp.policies.items, resp.total_count)
            })
            .await
    }

    /// 统一服务器模型（供到期提醒等通用逻辑复用）
    pub async fn list_servers(&self) -> Result<Vec<Server>> {
        let instances = self.list_instances().await?;
        Ok(instances
            .into_iter()
            .map(|i| {
                let id = i.instance_id.clone();
                Server {
                    id,
                    name: if i.instance_name.is_empty() {
                        i.instance_id
                    } else {
                        i.instance_name
                    },
                    region: self.rpc.region().to_string(),
                    status: i.status,
                    expired_at: parse_expired_time(&i.expired_time),
                }
            })
            .collect())
    }
}
