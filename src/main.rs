//! ops-console —— 服务商运维系统。
//!
//! 用法示例：
//!   ops-console projects
//!   ops-console snapshot --keep 2        # 快照轮转 + ECS 自动快照策略检查
//!   ops-console expiry --days 30,15,3    # 服务器（SWAS + ECS）+ 域名到期提醒
//!   ops-console disk --threshold 90      # SWAS + ECS 磁盘占用检查（超阈值/数据缺失通知）
//!   ops-console --project demo --provider aliyun snapshot --keep 2
//!
//! 未指定 --project 时遍历全部项目；未指定 --provider 时执行项目内全部服务商；
//! snapshot / expiry 对目标范围内的全部实例执行。

mod cloud;
mod config;
mod notify;
mod ops;

use clap::{Parser, Subcommand};
use std::path::Path;

use crate::cloud::driver::{Command as DriverCommand, ProviderDriver};

#[derive(Parser)]
#[command(
    name = "ops-console",
    version,
    about = "服务商运维系统：多服务商统一运维操作",
    long_about = "服务商运维系统\n\n起步：阿里云轻量服务器快照轮转\n扩展：实现 cloud::CloudProvider trait 即可接入新服务商"
)]
struct Cli {
    /// 配置目录（内含 project.yml / notify.yml）
    /// global：可写在子命令后（如 serve --config DIR），也可在顶层（ops-console --config DIR serve）
    #[arg(long, default_value = "config", global = true)]
    config: String,

    /// 目标项目名（默认全部项目）
    #[arg(long, global = true)]
    project: Option<String>,

    /// 只执行指定服务商（默认项目内全部服务商）
    #[arg(long, global = true)]
    provider: Option<String>,

    /// 日志级别 (error|warn|info|debug)
    #[arg(long, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出配置中的项目
    Projects,

    /// 快照轮转：删旧建新，对目标项目全部（或 --provider 指定）服务商下的全部实例执行
    Snapshot {
        /// 保留的快照份数（阿里云单台上限 3，建议 2）
        #[arg(long, default_value_t = 2)]
        keep: usize,

        /// 等待快照就绪的超时时间（分钟）
        #[arg(long, default_value_t = 30)]
        wait_minutes: u64,
    },

    /// 到期提醒：检查服务器（SWAS + ECS）与域名到期，命中阈值（或已过期）时输出并通知
    Expiry {
        /// 提醒阈值（天），逗号分隔
        #[arg(long, default_value = "30,15,3")]
        days: String,
    },

    /// 磁盘占用检查：使用率超阈值（默认 90%）或数据缺失时输出并通知
    Disk {
        /// 磁盘使用率阈值（%），达到则告警
        #[arg(long, default_value_t = 90.0)]
        threshold: f64,
    },

    /// 启动 Web 管理界面（配置管理 + 手动运行子命令）
    Serve {
        /// 监听地址（默认 127.0.0.1:8899）
        #[arg(long, default_value = "127.0.0.1:8899")]
        addr: String,
        /// 访问令牌（默认: env OPS_CONSOLE_TOKEN > serve.yml，空则自动生成保存）
        #[arg(long)]
        token: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 日志
    let filter = format!("ops_console={}", cli.log);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    // serve 不依赖有效的 project.yml（UI 需引导首次配置/修复损坏配置），提前分发，避免 Config::load 失败阻断
    if let Command::Serve { addr, token } = &cli.command {
        return crate::serve::run(&std::path::PathBuf::from(&cli.config), addr, token.clone()).await;
    }

    let cfg = config::Config::load(Path::new(&cli.config))?;

    let project_errors = match cli.command {
        Command::Projects => {
            for p in &cfg.projects {
                let kinds: Vec<&str> = p.providers.keys().map(|s| s.as_str()).collect();
                let desc = p.description.as_deref().unwrap_or("");
                println!("{:<20} providers: {:<24} {}", p.name, kinds.join(", "), desc);
            }
            Vec::new()
        }
        Command::Snapshot { keep, wait_minutes } => {
            let targets = select_projects(&cfg, cli.project.as_deref())?;
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut errors = Vec::new();
            for project in targets {
                println!("\n===== 项目: {} =====", project.name);
                let drivers = match drivers_for_project(project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 执行失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                let mut autosnap: Vec<(String, String, ops::ecs::AutoSnapshotStatus)> = Vec::new();
                for driver in drivers {
                    println!("-- 服务商: {}", driver.kind());
                    if driver.commands().contains(&DriverCommand::Snapshot) {
                        if let Err(e) = driver.rotate(&cfg, project, keep, wait_minutes).await {
                            println!("项目 {} 执行失败: {e:#}", project.name);
                            errors.push(project.name.clone());
                        }
                    }
                    // ECS 自动快照策略检查：巡检随快照轮转一起跑（未开启的实例汇总通知）
                    if driver.commands().contains(&DriverCommand::EcsAutosnapshot) {
                        match driver.ecs_autosnapshot(&cfg, project).await {
                            Ok(list) => autosnap.extend(list.into_iter().map(|s| {
                                (project.name.clone(), driver.kind().to_string(), s)
                            })),
                            Err(e) => {
                                println!("项目 {} ECS 自动快照检查失败: {e:#}", project.name);
                                errors.push(format!("{} (ECS 检查)", project.name));
                            }
                        }
                    }
                }
                if !autosnap.is_empty() {
                    println!("{}", ops::ecs::render_autosnapshot(&autosnap));
                    // 只通知未开启的实例
                    let unprotected: Vec<_> = autosnap
                        .iter()
                        .filter(|(_, _, s)| !s.protected())
                        .cloned()
                        .collect();
                    if !unprotected.is_empty() {
                        if let Some(n) = &notifier {
                            let title = format!("ECS 自动快照检查: {} 台未开启", unprotected.len());
                            let text = ops::ecs::render_autosnapshot(&unprotected);
                            if let Err(e) = n.send(&title, &text).await {
                                tracing::warn!("通知发送失败: {e}");
                            }
                        }
                    }
                }
            }
            errors
        }
        Command::Expiry { days } => {
            let thresholds = parse_thresholds(&days)?;

            let targets = select_projects(&cfg, cli.project.as_deref())?;

            // 汇总全部项目/服务商的命中提醒（服务器 SWAS+ECS + 域名），最后发一条通知（避免刷屏）
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut alerts: Vec<(String, String, ops::expiry::ExpiryAlert)> = Vec::new();
            let mut domain_alerts: Vec<(String, String, ops::expiry::DomainAlert)> = Vec::new();
            let mut errors = Vec::new();
            for project in &targets {
                println!("\n===== 项目: {} =====", project.name);
                let drivers = match drivers_for_project(project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 检查失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                for driver in drivers {
                    if driver.commands().contains(&DriverCommand::Expiry) {
                        println!("-- 服务商: {}", driver.kind());
                        match driver.expiry(&cfg, project, &thresholds).await {
                            Ok(list) => alerts.extend(list.into_iter().map(|a| {
                                (project.name.clone(), driver.kind().to_string(), a)
                            })),
                            Err(e) => {
                                println!("服务商 {} 检查失败: {e:#}", driver.kind());
                                errors.push(driver.kind().to_string());
                            }
                        }
                    }
                    // ECS 到期检查：label 标记 {kind}-ecs 便于区分
                    if driver.commands().contains(&DriverCommand::EcsExpiry) {
                        println!("-- ECS 到期检查");
                        let label = format!("{}-ecs", driver.kind());
                        match driver.ecs_expiry(&cfg, project, &thresholds).await {
                            Ok(list) => alerts.extend(list.into_iter().map(|a| {
                                (project.name.clone(), label.clone(), a)
                            })),
                            Err(e) => {
                                println!("ECS 到期检查失败: {e:#}");
                                errors.push(label.clone());
                            }
                        }
                    }
                    // 域名到期检查（账号级全局资源，不受地域影响）
                    if driver.commands().contains(&DriverCommand::ExpiryDomain) {
                        match driver.domain_expiry(&cfg, project, &thresholds).await {
                            Ok(list) => domain_alerts.extend(list.into_iter().map(|a| {
                                (project.name.clone(), driver.kind().to_string(), a)
                            })),
                            Err(e) => {
                                if cloud::aliyun::is_permission_error(&e) {
                                    // 账号未开通/未授权域名服务（如无域名资源）→ 跳过，不视为失败
                                    println!("  跳过域名检查（无 domain 权限，可能未注册域名）");
                                } else {
                                    println!("域名到期检查失败: {e:#}");
                                    errors.push(driver.kind().to_string());
                                }
                            }
                        }
                    }
                }
            }

            if !alerts.is_empty() || !domain_alerts.is_empty() {
                let mut text = String::new();
                if !alerts.is_empty() {
                    text.push_str(&ops::expiry::render(&alerts));
                }
                if !domain_alerts.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&ops::expiry::render_domains(&domain_alerts));
                }
                println!("{text}");
                if let Some(n) = &notifier {
                    let mut parts = Vec::new();
                    if !alerts.is_empty() {
                        parts.push(format!("{} 台服务器", alerts.len()));
                    }
                    if !domain_alerts.is_empty() {
                        parts.push(format!("{} 个域名", domain_alerts.len()));
                    }
                    let title = format!("到期提醒: {}", parts.join(", "));
                    if let Err(e) = n.send(&title, &text).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            } else {
                let list = thresholds
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("全部资源均在安全期内（{list} 天内无到期）");
            }

            if !errors.is_empty() {
                anyhow::bail!("以下服务商检查失败: {}", errors.join(", "));
            }
            Vec::new()
        }
        Command::Serve { .. } => {
            unreachable!("Serve 已在 Config::load 之前分发，不应到达此处")
        }
        Command::Disk { threshold } => {
            if !(threshold > 0.0 && threshold <= 100.0) {
                anyhow::bail!("--threshold 参数无效: {threshold}（范围 0 < 阈值 <= 100）");
            }

            let targets = select_projects(&cfg, cli.project.as_deref())?;
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut over: Vec<(String, String, ops::disk::DiskStatus)> = Vec::new();
            let mut missing: Vec<(String, String, ops::disk::DiskStatus)> = Vec::new();
            let mut errors = Vec::new();
            for project in &targets {
                println!("\n===== 项目: {} =====", project.name);
                let drivers = match drivers_for_project(project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 磁盘检查失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                for driver in drivers {
                    if !driver.commands().contains(&DriverCommand::Disk) {
                        continue;
                    }
                    match driver.disk(&cfg, project, threshold).await {
                        Ok(groups) => {
                            for g in groups.groups {
                                // 资源族（SWAS / ECS）全失败 → 记为该 label 的失败
                                if let Some(err) = g.error {
                                    println!("服务商 {} 磁盘检查失败: {err}", g.label);
                                    errors.push(g.label.clone());
                                    continue;
                                }
                                over.extend(g.over.into_iter().map(|s| {
                                    (project.name.clone(), g.label.clone(), s)
                                }));
                                missing.extend(g.missing.into_iter().map(|s| {
                                    (project.name.clone(), g.label.clone(), s)
                                }));
                            }
                        }
                        Err(e) => {
                            println!("服务商 {} 磁盘检查失败: {e:#}", driver.kind());
                            errors.push(driver.kind().to_string());
                        }
                    }
                }
            }

            if !over.is_empty() || !missing.is_empty() {
                let text = ops::disk::render_disk(&over, &missing);
                println!("{text}");
                if let Some(n) = &notifier {
                    let title = ops::disk::title(&over, &missing);
                    if let Err(e) = n.send(&title, &text).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            } else {
                println!("全部实例磁盘正常");
            }

            if !errors.is_empty() {
                anyhow::bail!("以下服务商磁盘检查失败: {}", errors.join(", "));
            }
            Vec::new()
        }
    };

    if !project_errors.is_empty() {
        anyhow::bail!("以下项目执行失败: {}", project_errors.join(", "));
    }
    Ok(())
}

/// 解析 --days 阈值列表（如 "30,15,3"）
fn parse_thresholds(days: &str) -> anyhow::Result<Vec<i64>> {
    let thresholds: Vec<i64> = days
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>()
                .map_err(|_| anyhow::anyhow!("--days 参数无效: {s:?}（格式如 30,15,3）"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if thresholds.is_empty() {
        anyhow::bail!("--days 至少需要一个阈值（如 30,15,3）");
    }
    Ok(thresholds)
}

/// 项目内应执行的驱动列表：--provider 过滤 + 项目已配置校验。
///
/// - `--provider` 指定未注册服务商 → 报「尚未实现」
/// - `--provider` 指定项目未配置 → 报「未配置」
/// - 未指定：遍历项目已配置的服务商；未实现的服务商 → 打印警告并跳过（宽容）
fn drivers_for_project<'a>(
    project: &'a config::Project,
    filter: Option<&str>,
) -> anyhow::Result<Vec<&'a dyn ProviderDriver>> {
    use crate::cloud::driver::{driver, supported_kinds};

    match filter {
        Some(k) => {
            if driver(k).is_none() {
                anyhow::bail!(
                    "服务商 {k:?} 尚未实现（目前仅支持: {}）",
                    supported_kinds().join(", ")
                );
            }
            if !project.providers.contains_key(k) {
                anyhow::bail!(
                    "项目 {} 未配置服务商 {k:?}（可用: {}）",
                    project.name,
                    project.providers.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(vec![driver(k).unwrap()])
        }
        None => {
            let mut out = Vec::new();
            for k in project.providers.keys() {
                match driver(k) {
                    Some(d) => out.push(d),
                    None => println!(
                        "  跳过未实现的服务商: {k}（当前仅支持: {}）",
                        supported_kinds().join(", ")
                    ),
                }
            }
            Ok(out)
        }
    }
}

/// 目标项目列表（--project 过滤，默认全部）
fn select_projects<'a>(
    cfg: &'a config::Config,
    name: Option<&str>,
) -> anyhow::Result<Vec<&'a config::Project>> {
    match name {
        Some(n) => Ok(vec![cfg.select_project(Some(n))?]),
        None => Ok(cfg.projects.iter().collect()),
    }
}

mod serve;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// --config 是 global 参数：子命令前后两种位置都应被接受（serve --config DIR 是用户直觉写法）
    #[test]
    fn test_config_global_accepts_both_positions() {
        // 顶层位置（传统写法）
        let top = Cli::try_parse_from(["ops-console", "--config", "a", "serve", "--addr", "127.0.0.1:1"]).unwrap();
        assert_eq!(top.config, "a");
        assert!(matches!(top.command, Command::Serve { .. }));
        // 子命令后位置（serve --config DIR）
        let sub = Cli::try_parse_from(["ops-console", "serve", "--config", "b", "--addr", "127.0.0.1:1"]).unwrap();
        assert_eq!(sub.config, "b");
        // 其他子命令后位置
        let projects = Cli::try_parse_from(["ops-console", "projects", "--config", "c"]).unwrap();
        assert_eq!(projects.config, "c");
    }

    #[test]
    fn test_drivers_for_project() {
        use crate::config::{Config, Project, ProviderConfig};
        use std::collections::BTreeMap;

        let project = Project {
            name: "demo".into(),
            description: None,
            providers: BTreeMap::from([
                ("aliyun".into(), ProviderConfig::default()),
                ("tencent".into(), ProviderConfig::default()),
            ]),
        };
        let cfg = Config {
            projects: vec![project],
            notify: Default::default(),
        };
        let p = &cfg.projects[0];

        // --provider aliyun（已配置）→ 命中
        let ds = drivers_for_project(p, Some("aliyun")).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].kind(), "aliyun");

        // --provider tencent（未实现）→ 报「尚未实现」
        let err = match drivers_for_project(p, Some("tencent")) {
            Ok(_) => panic!("应返回错误"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("尚未实现"));

        // --provider 已实现但项目未配置 → 报「未配置」
        let cfg2 = Config {
            projects: vec![Project {
                name: "other".into(),
                description: None,
                providers: BTreeMap::new(),
            }],
            notify: Default::default(),
        };
        let err = match drivers_for_project(&cfg2.projects[0], Some("aliyun")) {
            Ok(_) => panic!("应返回错误"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("未配置"));

        // 未指定 --provider → 只返回已实现服务商（tencent 打印警告并跳过）
        let ds = drivers_for_project(p, None).unwrap();
        let kinds: Vec<&str> = ds.iter().map(|d| d.kind()).collect();
        assert_eq!(kinds, vec!["aliyun"]);
    }
}
