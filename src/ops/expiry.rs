//! 服务器到期提醒：检查实例到期时间，命中阈值（默认 30/15/3 天）或已过期时提醒。
//!
//! 配合 cron 每天运行一次：剩余天数向上取整，精确命中阈值的只有一天，
//! 天然不会重复提醒；已过期（<= 0 天）则每次运行都会提醒（紧急，需人工处理）。

use crate::cloud::{CloudProvider, Server};
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
    let mut alerts = Vec::new();
    for server in provider.list_servers().await? {
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
    Ok(alerts)
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
            "- {project}/{kind}: {} ({}) 到期 {}，{tag}\n",
            a.server.name,
            a.server.id,
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
