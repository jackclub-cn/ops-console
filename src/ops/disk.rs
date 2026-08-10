//! 磁盘占用检查：SWAS 用 DescribeMonitorData + ListDisks，ECS 用云监控 diskusage_utilization。
//!
//! 超阈值（默认 90%）与数据缺失（Running 但查不到监控数据，疑似未装云监控插件）
//! 分别汇总，随 cron 每次运行都通知（无持久化，与 expiry 一致）。

use crate::cloud::aliyun::cms::{CmsClient, MetricPoint};
use crate::cloud::aliyun::ecs::EcsClient;
use crate::cloud::aliyun::swas::SwasClient;
use crate::cloud::Server;
use anyhow::Result;
use std::collections::HashMap;

/// 单个实例的磁盘占用状态
#[derive(Debug, Clone)]
pub struct DiskStatus {
    pub server: Server,
    /// 使用率（%）；None = 数据缺失
    pub utilization: Option<f64>,
    /// 已用字节（展示用）
    pub used_bytes: u64,
    /// 总容量字节（展示用）
    pub total_bytes: u64,
    /// 数据来源描述（SWAS: "系统盘"；ECS: 设备明细）
    pub detail: String,
}

impl DiskStatus {
    /// 数据缺失：Running 但查不到监控数据
    ///
    /// 设计文档规定的"数据缺失"语义访问器，由单元测试覆盖；
    /// 生产侧缺失分支以 `utilization: None` 直接构造列表，故允许未使用。
    #[allow(dead_code)]
    pub fn missing(&self) -> bool {
        self.utilization.is_none()
    }

    /// 是否达到/超过阈值（>=）
    pub fn over(&self, threshold: f64) -> bool {
        matches!(self.utilization, Some(u) if u >= threshold)
    }
}

/// ECS 侧：单实例的聚合结果（各设备中最大使用率 + 设备明细）
#[derive(Debug, Clone)]
pub struct InstanceUsage {
    /// 设备中最大的 Average 使用率（%）
    pub max: f64,
    /// (设备名, 使用率) 明细，按使用率降序
    pub devices: Vec<(String, f64)>,
}

/// 按 instanceId 聚合监控数据点：每实例取各设备最大 Average 作为使用率。
pub fn aggregate_by_instance(points: Vec<MetricPoint>) -> HashMap<String, InstanceUsage> {
    let mut map: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for p in points {
        map.entry(p.instance_id.clone()).or_default().push((p.device, p.average));
    }
    map.into_iter()
        .map(|(id, mut devices)| {
            devices.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let max = devices.first().map(|(_, v)| *v).unwrap_or(0.0);
            (id, InstanceUsage { max, devices })
        })
        .collect()
}

/// 设备明细："设备: /dev/vda1 91.2%, /dev/vdb1 30.1%"；设备名全空 → "系统盘 x.x%"
fn format_devices(devices: &[(String, f64)]) -> String {
    let parts: Vec<String> = devices
        .iter()
        .filter(|(d, _)| !d.is_empty())
        .map(|(d, v)| format!("{d} {v:.1}%"))
        .collect();
    if parts.is_empty() {
        let v = devices.first().map(|(_, v)| *v).unwrap_or(0.0);
        format!("系统盘 {v:.1}%")
    } else {
        format!("设备: {}", parts.join(", "))
    }
}

/// 字节 → 可读字符串（B/KB/MB/GB/TB，>=1024 进位，1 位小数）
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 渲染检查结果：超阈值表 + 数据缺失表（各表为空则省略对应部分）。
pub fn render_disk(
    over: &[(String, String, DiskStatus)],
    missing: &[(String, String, DiskStatus)],
) -> String {
    let mut out = String::new();
    if !over.is_empty() {
        out.push_str("=== 磁盘占用检查 ===\n");
        for (project, kind, s) in over {
            let pct = s.utilization.unwrap_or(0.0);
            // ECS 路径无字节信息（total=0），只输出 detail，避免误导性 "已用 0 B"
            let detail = if s.total_bytes > 0 {
                format!(
                    "已用 {}/{} {}",
                    format_bytes(s.used_bytes),
                    format_bytes(s.total_bytes),
                    s.detail
                )
            } else {
                s.detail.clone()
            };
            out.push_str(&format!(
                "- {project}/{kind}: {} ({}) {:.1}% ({detail})\n",
                s.server.name,
                s.server.id,
                pct,
            ));
        }
    }
    if !missing.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("=== 数据缺失 ===\n");
        for (project, kind, s) in missing {
            out.push_str(&format!(
                "- {project}/{kind}: {} ({}) 无磁盘监控数据（疑似未装云监控插件）\n",
                s.server.name, s.server.id
            ));
        }
    }
    out
}

/// 通知标题：`磁盘占用检查: N 台超阈值, M 台数据缺失`（空的部分省略）
pub fn title(over: &[(String, String, DiskStatus)], missing: &[(String, String, DiskStatus)]) -> String {
    let mut parts = Vec::new();
    if !over.is_empty() {
        parts.push(format!("{} 台超阈值", over.len()));
    }
    if !missing.is_empty() {
        parts.push(format!("{} 台数据缺失", missing.len()));
    }
    if parts.is_empty() {
        "磁盘占用检查: 全部正常".to_string()
    } else {
        format!("磁盘占用检查: {}", parts.join(", "))
    }
}

// ---------- 网络面 check 函数 ----------

/// 构建展示用 Server（region 留空：render 不使用）
fn server_from(name: String, id: String, status: &str) -> Server {
    Server {
        id,
        name,
        region: String::new(),
        status: status.to_string(),
        expired_at: None,
    }
}

/// 磁盘类型 → 中文标注（兼容真实 API 返回的小写变体 system/data）
fn disk_type_label(disk_type: &str) -> String {
    match disk_type.to_ascii_lowercase().as_str() {
        "system" => "系统盘".to_string(),
        "data" => "数据盘".to_string(),
        other => format!("磁盘({other})"),
    }
}

/// 查询单台轻量实例的磁盘用量：返回 (已用 bytes, 总 bytes, 来源描述)；无数据 → None。
async fn disk_usage_swas(client: &SwasClient, instance_id: &str) -> Result<Option<(u64, u64, String)>> {
    let used = match client.disk_usage_used(instance_id).await? {
        Some(u) => u,
        None => return Ok(None),
    };
    let disks = client.list_disks(instance_id).await?;
    // 优先系统盘（真实 API 返回小写 system），退而求其次第一块盘
    let disk = disks
        .iter()
        .find(|d| d.disk_type.eq_ignore_ascii_case("System"))
        .or_else(|| disks.first());
    let Some(disk) = disk else { return Ok(None) };
    if disk.size <= 0 {
        return Ok(None);
    }
    let total = disk.size as u64 * 1024 * 1024 * 1024;
    let detail = disk_type_label(&disk.disk_type);
    Ok(Some((used, total, detail)))
}

/// SWAS 磁盘检查：只查 Running 实例；返回 (超阈值列表, 数据缺失列表)。
/// 单实例查询失败 → println 告警并跳过（不归入缺失，避免把 API 错误误报成插件问题）。
pub async fn check_swas_disk(
    client: &SwasClient,
    threshold: f64,
) -> Result<(Vec<DiskStatus>, Vec<DiskStatus>)> {
    let instances = client.list_instances().await?;
    let total = instances.len();
    let running: Vec<_> = instances.into_iter().filter(|i| i.status == "Running").collect();
    println!("  轻量实例 {total} 台，跳过 {} 台非 Running", total - running.len());

    let mut over = Vec::new();
    let mut missing = Vec::new();
    for inst in running {
        let id = inst.instance_id.clone();
        let name = if inst.instance_name.is_empty() {
            id.clone()
        } else {
            inst.instance_name.clone()
        };
        match disk_usage_swas(client, &id).await {
            Ok(Some((used, total_bytes, detail))) => {
                let utilization = used as f64 / total_bytes as f64 * 100.0;
                let status = DiskStatus {
                    server: server_from(name.clone(), id.clone(), &inst.status),
                    utilization: Some(utilization),
                    used_bytes: used,
                    total_bytes,
                    detail,
                };
                if status.over(threshold) {
                    over.push(status);
                }
            }
            Ok(None) => missing.push(DiskStatus {
                server: server_from(name.clone(), id.clone(), &inst.status),
                utilization: None,
                used_bytes: 0,
                total_bytes: 0,
                detail: String::new(),
            }),
            Err(e) => println!("    实例 {name} ({id}) 磁盘数据查询失败: {e:#}"),
        }
    }
    Ok((over, missing))
}

/// ECS 磁盘检查：一次 DescribeMetricLast 拿地域内全部实例的 diskusage_utilization，
/// 按 instanceId 聚合设备取最大；只查 Running 实例；返回 (超阈值, 数据缺失)。
pub async fn check_ecs_disk(
    client: &EcsClient,
    cms: &CmsClient,
    threshold: f64,
) -> Result<(Vec<DiskStatus>, Vec<DiskStatus>)> {
    let servers = client.list_servers().await?;
    let total = servers.len();
    let running: Vec<Server> = servers.into_iter().filter(|s| s.status == "Running").collect();
    println!("  ECS 实例 {total} 台，跳过 {} 台非 Running", total - running.len());

    let points = cms
        .describe_metric_last("acs_ecs_dashboard", "diskusage_utilization", None)
        .await?;
    let agg = aggregate_by_instance(points);

    let mut over = Vec::new();
    let mut missing = Vec::new();
    for server in running {
        match agg.get(&server.id) {
            Some(u) => {
                let status = DiskStatus {
                    server,
                    utilization: Some(u.max),
                    used_bytes: 0,
                    total_bytes: 0,
                    detail: format_devices(&u.devices),
                };
                if status.over(threshold) {
                    over.push(status);
                }
            }
            None => missing.push(DiskStatus {
                server,
                utilization: None,
                used_bytes: 0,
                total_bytes: 0,
                detail: String::new(),
            }),
        }
    }
    Ok((over, missing))
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

    fn mk_status(name: &str, utilization: Option<f64>, used: u64, total: u64, detail: &str) -> DiskStatus {
        DiskStatus {
            server: mk_server(name),
            utilization,
            used_bytes: used,
            total_bytes: total,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn test_over_boundary() {
        // 达到 90.0 即告警（>=）
        assert!(mk_status("a", Some(90.0), 0, 0, "").over(90.0));
        assert!(mk_status("a", Some(91.2), 0, 0, "").over(90.0));
        // 89.9 不告警
        assert!(!mk_status("a", Some(89.9), 0, 0, "").over(90.0));
        // 数据缺失不归入超阈值，单独走 missing()
        assert!(!mk_status("a", None, 0, 0, "").over(90.0));
        assert!(mk_status("a", None, 0, 0, "").missing());
    }

    #[test]
    fn test_aggregate_by_instance() {
        use crate::cloud::aliyun::cms::MetricPoint;
        let pts = vec![
            MetricPoint { instance_id: "i-1".into(), device: "/dev/vda1".into(), average: 91.2 },
            MetricPoint { instance_id: "i-1".into(), device: "/dev/vdb1".into(), average: 30.1 },
            MetricPoint { instance_id: "i-2".into(), device: "/dev/vda1".into(), average: 88.0 },
        ];
        let agg = aggregate_by_instance(pts);
        assert_eq!(agg.len(), 2);
        let a = &agg["i-1"];
        assert_eq!(a.max, 91.2);
        assert_eq!(a.devices.len(), 2);
        // 设备按使用率降序
        assert_eq!(a.devices[0].0, "/dev/vda1");
        assert_eq!(agg["i-2"].max, 88.0);
    }

    #[test]
    fn test_format_devices() {
        let d = vec![("/dev/vda1".to_string(), 91.2), ("/dev/vdb1".to_string(), 30.1)];
        assert_eq!(format_devices(&d), "设备: /dev/vda1 91.2%, /dev/vdb1 30.1%");
        // 设备名为空（非常规指标）→ 退化为 "系统盘 x.x%"
        let d = vec![("".to_string(), 95.0)];
        assert_eq!(format_devices(&d), "系统盘 95.0%");
    }

    #[test]
    fn test_disk_type_label() {
        assert_eq!(disk_type_label("System"), "系统盘");
        assert_eq!(disk_type_label("system"), "系统盘"); // 真实 API 返回小写
        assert_eq!(disk_type_label("Data"), "数据盘");
        assert_eq!(disk_type_label("data"), "数据盘");
        assert_eq!(disk_type_label("unknown"), "磁盘(unknown)");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(50 * 1024 * 1024 * 1024), "50.0 GB");
    }

    #[test]
    fn test_render_disk() {
        let over = vec![(
            "demo".to_string(),
            "aliyun".to_string(),
            mk_status("web", Some(91.2), 45 * 1024 * 1024 * 1024, 50 * 1024 * 1024 * 1024, "系统盘"),
        )];
        let missing = vec![(
            "demo".to_string(),
            "aliyun-ecs".to_string(),
            mk_status("cache", None, 0, 0, ""),
        )];
        let text = render_disk(&over, &missing);
        assert!(text.contains("web (i-web) 91.2% (已用 45.0 GB/50.0 GB 系统盘)"));
        assert!(text.contains("=== 数据缺失 ==="));
        assert!(text.contains("cache (i-cache) 无磁盘监控数据（疑似未装云监控插件）"));

        // ECS 风格：total_bytes=0（无字节信息）→ 只输出 detail，不输出误导性 "已用 0 B"
        let over_ecs = vec![(
            "prod".to_string(),
            "aliyun-ecs".to_string(),
            mk_status("db", Some(95.0), 0, 0, "设备: /dev/vda1 95.0%"),
        )];
        let text_ecs = render_disk(&over_ecs, &[]);
        assert!(text_ecs.contains("db (i-db) 95.0% (设备: /dev/vda1 95.0%)"));
        assert!(!text_ecs.contains("已用 0 B"));

        // 全空 → 空字符串
        assert_eq!(render_disk(&[], &[]), "");
    }

    #[test]
    fn test_title() {
        let over = vec![("demo".into(), "aliyun".into(), mk_status("a", Some(91.0), 0, 0, ""))];
        let missing = vec![("demo".into(), "aliyun-ecs".into(), mk_status("b", None, 0, 0, ""))];
        assert_eq!(title(&over, &missing), "磁盘占用检查: 1 台超阈值, 1 台数据缺失");
        assert_eq!(title(&over, &[]), "磁盘占用检查: 1 台超阈值");
        assert_eq!(title(&[], &missing), "磁盘占用检查: 1 台数据缺失");
        assert_eq!(title(&[], &[]), "磁盘占用检查: 全部正常");
    }
}
