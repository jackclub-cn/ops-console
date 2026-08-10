//! ECS 运维检查：自动快照策略是否开启。
//!
//! 判断依据：`DescribeDisks` 返回每块云盘的 `AutoSnapshotPolicyId`（已绑定策略），
//! 实例任一磁盘绑定了策略 = 已开启自动快照（系统盘+数据盘都在保护内）。

use crate::cloud::aliyun::ecs::EcsClient;
use crate::cloud::Server;
use anyhow::Result;

/// 单块云盘的自动快照绑定状态
#[derive(Debug, Clone)]
pub struct DiskSnapshotStatus {
    pub disk_id: String,
    /// system | data
    pub disk_type: String,
    /// 绑定的自动快照策略 ID（空 = 未绑定）
    pub policy_id: String,
}

/// 单个实例的自动快照检查结果
#[derive(Debug, Clone)]
pub struct AutoSnapshotStatus {
    pub server: Server,
    pub disks: Vec<DiskSnapshotStatus>,
    /// 命中的策略摘要（去重，如 "每日快照（触发 00:00，保留 7 天）"）
    pub policy_summaries: Vec<String>,
}

impl AutoSnapshotStatus {
    /// 任一磁盘绑定了自动快照策略 = 已开启
    pub fn protected(&self) -> bool {
        self.disks.iter().any(|d| !d.policy_id.is_empty())
    }
}

/// 检查地域内全部 ECS 实例的自动快照开启情况。
/// 每实例一次 DescribeDisks（磁盘数很少，无分页压力）。
pub async fn check_auto_snapshot(client: &EcsClient) -> Result<Vec<AutoSnapshotStatus>> {
    let servers = client.list_servers().await?;
    let policies = client.list_auto_snapshot_policies().await?;
    let mut out = Vec::new();
    for server in servers {
        let disks = client.list_disks(&server.id).await?;
        let mut disk_status = Vec::new();
        let mut policy_summaries = Vec::new();
        for d in disks {
            let pid = d.auto_snapshot_policy_id.clone();
            if !pid.is_empty() {
                if let Some(p) = policies.iter().find(|p| p.policy_id == pid) {
                    let summary = p.summary();
                    if !policy_summaries.contains(&summary) {
                        policy_summaries.push(summary);
                    }
                }
            }
            disk_status.push(DiskSnapshotStatus {
                disk_id: d.disk_id,
                disk_type: d.disk_type,
                policy_id: pid,
            });
        }
        out.push(AutoSnapshotStatus {
            server,
            disks: disk_status,
            policy_summaries,
        });
    }
    Ok(out)
}

/// 渲染检查结果：`项目/服务商 实例名 (ID) [已开启|未开启] 详情`
pub fn render_autosnapshot(items: &[(String, String, AutoSnapshotStatus)]) -> String {
    let mut out = String::from("=== ECS 自动快照检查 ===\n");
    for (project, kind, s) in items {
        let mark = if s.protected() { "已开启" } else { "未开启" };
        let detail = if s.protected() {
            format!("策略: {}", s.policy_summaries.join("，"))
        } else {
            let disks: Vec<String> = s
                .disks
                .iter()
                .map(|d| format!("{} ({})", d.disk_id, d.disk_type))
                .collect();
            format!("磁盘: {}", disks.join(", "))
        };
        out.push_str(&format!(
            "- {project}/{kind}: {} ({}) [{mark}] {detail}\n",
            s.server.name, s.server.id
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_server(name: &str) -> Server {
        Server {
            id: format!("i-{name}"),
            name: name.to_string(),
            region: "cn-shenzhen".to_string(),
            status: "Running".to_string(),
            expired_at: None,
        }
    }

    fn mk_disk(disk_id: &str, disk_type: &str, policy_id: &str) -> DiskSnapshotStatus {
        DiskSnapshotStatus {
            disk_id: disk_id.to_string(),
            disk_type: disk_type.to_string(),
            policy_id: policy_id.to_string(),
        }
    }

    #[test]
    fn test_protected_any_disk() {
        let protected = AutoSnapshotStatus {
            server: mk_server("a"),
            disks: vec![mk_disk("d-1", "system", "sp-1")],
            policy_summaries: vec!["每日快照（触发 00:00，保留 7 天）".into()],
        };
        assert!(protected.protected());

        // 数据盘未绑定、系统盘已绑定 → 已开启
        let mixed = AutoSnapshotStatus {
            server: mk_server("b"),
            disks: vec![mk_disk("d-1", "system", "sp-1"), mk_disk("d-2", "data", "")],
            policy_summaries: vec!["每日快照".into()],
        };
        assert!(mixed.protected());

        let unprotected = AutoSnapshotStatus {
            server: mk_server("c"),
            disks: vec![mk_disk("d-1", "system", ""), mk_disk("d-2", "data", "")],
            policy_summaries: vec![],
        };
        assert!(!unprotected.protected());
    }

    #[test]
    fn test_render_autosnapshot() {
        let items = vec![
            (
                "demo".to_string(),
                "aliyun".to_string(),
                AutoSnapshotStatus {
                    server: mk_server("web"),
                    disks: vec![mk_disk("d-1", "system", "sp-1")],
                    policy_summaries: vec!["每日快照（触发 00:00，保留 7 天）".into()],
                },
            ),
            (
                "demo".to_string(),
                "aliyun".to_string(),
                AutoSnapshotStatus {
                    server: mk_server("db"),
                    disks: vec![mk_disk("d-2", "system", "")],
                    policy_summaries: vec![],
                },
            ),
        ];
        let text = render_autosnapshot(&items);
        assert!(text.contains("web (i-web) [已开启] 策略: 每日快照（触发 00:00，保留 7 天）"));
        assert!(text.contains("db (i-db) [未开启] 磁盘: d-2 (system)"));
    }
}
