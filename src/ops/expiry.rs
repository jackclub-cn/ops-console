//! 服务器到期提醒：检查实例到期时间，命中阈值（默认 30/15/3 天）或已过期时提醒。
//!
//! 配合 cron 每天运行一次：剩余天数向上取整，精确命中阈值的只有一天，
//! 天然不会重复提醒；已过期（<= 0 天）则每次运行都会提醒（紧急，需人工处理）。

use crate::cloud::{CloudProvider, Server};
use crate::cloud::aliyun::domain::DomainInfo;
use anyhow::Result;
use chrono::{DateTime, FixedOffset, Utc};

/// 一条到期提醒
#[derive(Debug, Clone)]
pub struct ExpiryAlert {
    pub server: Server,
    /// 到期时间（UTC）
    pub expired_at: DateTime<Utc>,
    /// 剩余天数（向上取整）；<= 0 表示已过期（负数为已过期天数）
    pub days_left: i64,
}

/// 剩余天数：正数向上取整（剩 29.5 天 → 30），已过期向下取整（过期 0.5 天 → -1）。
pub fn days_left(expired_at: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let d = expired_at.signed_duration_since(now).num_seconds() as f64 / 86400.0;
    if d >= 0.0 {
        d.ceil() as i64
    } else {
        d.floor() as i64
    }
}

/// 检查服务商下全部实例，返回命中阈值（或已过期）的提醒列表。
/// `now` 参数化便于测试。
pub async fn check<P: CloudProvider + ?Sized>(
    provider: &P,
    thresholds: &[i64],
    now: DateTime<Utc>,
) -> Result<Vec<ExpiryAlert>> {
    Ok(check_servers(provider.list_servers().await?, thresholds, now))
}

/// 纯逻辑：对服务器列表过滤命中提醒（ECS 等非 CloudProvider 客户端复用）。
pub fn check_servers(
    servers: Vec<Server>,
    thresholds: &[i64],
    now: DateTime<Utc>,
) -> Vec<ExpiryAlert> {
    let mut alerts = Vec::new();
    for server in servers {
        // 无到期时间（按量付费等）跳过
        let Some(expired_at) = server.expired_at else { continue };
        let d = days_left(expired_at, now);
        if d <= 0 || thresholds.contains(&d) {
            alerts.push(ExpiryAlert {
                server,
                expired_at,
                days_left: d,
            });
        }
    }
    alerts
}

/// 一条域名到期提醒
#[derive(Debug, Clone)]
pub struct DomainAlert {
    pub domain: String,
    pub expired_at: DateTime<Utc>,
    pub days_left: i64,
    pub auto_renew: bool,
}

/// 检查账号下全部域名，返回命中阈值（或已过期）的提醒列表。
pub async fn check_domains(
    client: &crate::cloud::aliyun::domain::DomainClient,
    thresholds: &[i64],
    now: DateTime<Utc>,
) -> Result<Vec<DomainAlert>> {
    Ok(check_domain_list(client.list_domains().await?, thresholds, now))
}

/// 纯逻辑：对域名列表过滤命中提醒（无到期时间 / 解析失败的跳过）。
pub fn check_domain_list(
    domains: Vec<DomainInfo>,
    thresholds: &[i64],
    now: DateTime<Utc>,
) -> Vec<DomainAlert> {
    let mut out = Vec::new();
    for d in domains {
        let Some(expired_at) = d.expired_at else { continue };
        let dl = days_left(expired_at, now);
        if dl <= 0 || thresholds.contains(&dl) {
            out.push(DomainAlert {
                domain: d.domain_name,
                expired_at,
                days_left: dl,
                auto_renew: d.auto_renew,
            });
        }
    }
    out
}

/// 渲染域名提醒：`项目/服务商 域名：到期时间（北京时间），剩余 N 天 / 已过期 N 天 [自动续费已开启]`
pub fn render_domains(items: &[(String, String, DomainAlert)]) -> String {
    let bjt_offset = FixedOffset::east_opt(8 * 3600).expect("时区偏移构造失败");
    let mut out = String::from("=== 域名到期提醒 ===\n");
    for (project, kind, a) in items {
        let bjt = a.expired_at.with_timezone(&bjt_offset);
        let tag = if a.days_left <= 0 {
            format!("已过期 {} 天", -a.days_left)
        } else {
            format!("剩余 {} 天", a.days_left)
        };
        let renew = if a.auto_renew { " [自动续费已开启]" } else { "" };
        out.push_str(&format!(
            "- {project}/{kind}: {} 到期 {}，{tag}{renew}\n",
            a.domain,
            bjt.format("%Y-%m-%d %H:%M")
        ));
    }
    out
}

/// 渲染提醒列表：`项目/服务商 服务器名 (ID)：到期时间（北京时间），剩余 N 天 / 已过期 N 天`
pub fn render(items: &[(String, String, ExpiryAlert)]) -> String {
    let bjt_offset = FixedOffset::east_opt(8 * 3600).expect("时区偏移构造失败");
    let mut out = String::from("=== 服务器到期提醒 ===\n");
    for (project, kind, a) in items {
        let bjt = a.expired_at.with_timezone(&bjt_offset);
        let tag = if a.days_left <= 0 {
            format!("已过期 {} 天", -a.days_left)
        } else {
            format!("剩余 {} 天", a.days_left)
        };
        out.push_str(&format!(
            "- {project}/{kind}: {} ({}{}) 到期 {}，{tag}\n",
            a.server.name,
            a.server.id,
            crate::cloud::region_suffix(&a.server.region),
            bjt.format("%Y-%m-%d %H:%M")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(s: &str) -> DateTime<Utc> {
        Utc.from_utc_datetime(
            &chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap(),
        )
    }

    #[test]
    fn test_days_left() {
        let now = dt("2026-08-01T00:00:00");
        // 正好 30 天
        assert_eq!(days_left(dt("2026-08-31T00:00:00"), now), 30);
        // 剩 29.5 天 → 向上取整 30（仍命中 30 天阈值）
        assert_eq!(days_left(dt("2026-08-30T12:00:00"), now), 30);
        // 剩 30.5 天 → 31（未到 30 天窗口，不提醒）
        assert_eq!(days_left(dt("2026-08-31T12:00:00"), now), 31);
        // 今天到期 → 0
        assert_eq!(days_left(now, now), 0);
        // 已过期 → 负数
        assert_eq!(days_left(dt("2026-07-20T00:00:00"), now), -12);
        assert_eq!(days_left(dt("2026-07-20T12:00:00"), now), -12);
    }

    #[test]
    fn test_render_includes_region() {
        let now = dt("2026-08-01T00:00:00");
        let alert = ExpiryAlert {
            server: Server {
                id: "i-x".into(),
                name: "web".into(),
                region: "cn-hangzhou".into(),
                status: "Running".into(),
                expired_at: Some(now),
            },
            expired_at: now,
            days_left: 30,
        };
        let text = render(&[("demo".into(), "aliyun".into(), alert)]);
        assert!(text.contains("demo/aliyun: web (i-x, cn-hangzhou) 到期 2026-08-01 08:00，剩余 30 天"));
    }

    #[test]
    fn test_check_domain_list_filters() {
        let now = dt("2026-08-01T00:00:00");
        let mk = |name: &str, expired: Option<&str>, auto_renew: bool| DomainInfo {
            domain_name: name.to_string(),
            expired_at: expired.map(|s| dt(s)),
            auto_renew,
        };
        let domains = vec![
            mk("hit-30d.com", Some("2026-08-31T00:00:00"), false), // 剩 30 天 → 命中
            mk("expired.com", Some("2026-07-20T00:00:00"), true),   // 已过期 → 命中
            mk("safe.com", Some("2027-01-01T00:00:00"), false),     // 安全期 → 跳过
            mk("no-date.com", None, false),                          // 无到期时间 → 跳过
        ];
        let alerts = check_domain_list(domains, &[30, 15, 3], now);
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].domain, "hit-30d.com");
        assert_eq!(alerts[0].days_left, 30);
        assert_eq!(alerts[1].domain, "expired.com");
        assert!(alerts[1].auto_renew);
    }

    #[test]
    fn test_render_domains() {
        let hit = DomainAlert {
            domain: "hit.com".into(),
            expired_at: dt("2026-08-31T00:00:00"),
            days_left: 30,
            auto_renew: false,
        };
        let expired = DomainAlert {
            domain: "expired.com".into(),
            expired_at: dt("2026-07-20T00:00:00"),
            days_left: -12,
            auto_renew: true,
        };
        let text = render_domains(&[
            ("demo".into(), "aliyun".into(), hit),
            ("demo".into(), "aliyun".into(), expired),
        ]);
        assert!(text.contains("demo/aliyun: hit.com 到期 2026-08-31 08:00，剩余 30 天"));
        assert!(text.contains("demo/aliyun: expired.com 到期 2026-07-20 08:00，已过期 12 天 [自动续费已开启]"));
    }

    #[tokio::test]
    async fn test_check_filters_by_threshold() {
        // 用 mock 实现做纯逻辑验证
        let now = dt("2026-08-01T00:00:00");
        let mk = |name: &str, expired: Option<&str>| Server {
            id: format!("id-{name}"),
            name: name.to_string(),
            region: "cn-shenzhen".to_string(),
            status: "Running".to_string(),
            expired_at: expired.map(|s| dt(s)),
        };

        struct Mock(Vec<Server>);
        #[async_trait::async_trait]
        impl CloudProvider for Mock {
            async fn list_servers(&self) -> Result<Vec<Server>> {
                Ok(self.0.clone())
            }
            async fn list_snapshots(&self, _: &str) -> Result<Vec<crate::cloud::Snapshot>> {
                Ok(vec![])
            }
            async fn create_snapshot(&self, _: &str, _: &str) -> Result<String> {
                Ok("s".into())
            }
            async fn delete_snapshot(&self, _: &str) -> Result<()> {
                Ok(())
            }
            async fn wait_snapshot_ready(
                &self,
                _: &str,
                _: &str,
                _: std::time::Duration,
            ) -> Result<()> {
                Ok(())
            }
        }

        let servers = vec![
            mk("exp30", Some("2026-08-31T00:00:00")), // 30 天 → 命中
            mk("exp14", Some("2026-08-15T00:00:00")), // 14 天 → 不命中
            mk("expired", Some("2026-07-10T00:00:00")), // 已过期 → 命中
            mk("nopay", None),                        // 无到期 → 跳过
        ];
        let provider = Mock(servers);
        let alerts = check(&provider, &[30, 15, 3], now).await.unwrap();
        let names: Vec<&str> = alerts.iter().map(|a| a.server.name.as_str()).collect();
        assert_eq!(names, vec!["exp30", "expired"]);
        assert_eq!(alerts[0].days_left, 30);
        assert!(alerts[1].days_left < 0);
    }
}
