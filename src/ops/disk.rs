//! 磁盘占用检查：SWAS 用 DescribeMonitorData + ListDisks，ECS 用云监控 diskusage_utilization。
//!
//! 超阈值（默认 90%）与数据缺失（Running 但查不到监控数据，疑似未装云监控插件）
//! 分别汇总，随 cron 每次运行都通知（无持久化，与 expiry 一致）。

use crate::cloud::aliyun::cms::MetricPoint;
use crate::cloud::Server;
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
            out.push_str(&format!(
                "- {project}/{kind}: {} ({}) {:.1}% (已用 {}/{} {})\n",
                s.server.name,
                s.server.id,
                pct,
                format_bytes(s.used_bytes),
                format_bytes(s.total_bytes),
                s.detail
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
