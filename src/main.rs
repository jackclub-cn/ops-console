//! ops-console —— 服务商运维系统。
//!
//! 用法示例：
//!   ops-console projects
//!   ops-console snapshot --keep 2
//!   ops-console expiry --days 30,15,3
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
use std::time::Duration;

use crate::cloud::CloudProvider;

#[derive(Parser)]
#[command(
    name = "ops-console",
    version,
    about = "服务商运维系统：多服务商统一运维操作",
    long_about = "服务商运维系统\n\n起步：阿里云轻量服务器快照轮转\n扩展：实现 cloud::CloudProvider trait 即可接入新服务商"
)]
struct Cli {
    /// 配置目录（内含 project.yml / notify.yml）
    #[arg(long, default_value = "config")]
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

    /// 服务器到期提醒：命中阈值（或已过期）时输出并通知
    Expiry {
        /// 提醒阈值（天），逗号分隔
        #[arg(long, default_value = "30,15,3")]
        days: String,
    },

    /// ECS 运维检查：自动快照策略 + 到期提醒（复用 aliyun 配置的凭据与地域）
    Ecs {
        #[command(subcommand)]
        command: EcsCommand,
    },
}

#[derive(Subcommand)]
enum EcsCommand {
    /// 检查自动快照策略是否开启，未开启的实例汇总通知
    #[command(name = "autosnapshot")]
    AutoSnapshot,

    /// 到期提醒：命中阈值（或已过期）时输出并通知
    Expiry {
        /// 提醒阈值（天），逗号分隔
        #[arg(long, default_value = "30,15,3")]
        days: String,
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
            let targets: Vec<&config::Project> = match cli.project.as_deref() {
                Some(name) => vec![cfg.select_project(Some(name))?],
                None => cfg.projects.iter().collect(),
            };
            let mut errors = Vec::new();
            for project in targets {
                println!("\n===== 项目: {} =====", project.name);
                if let Err(e) = run_project_rotate(&cfg, project, cli.provider.as_deref(), keep, wait_minutes)
                    .await
                {
                    println!("项目 {} 执行失败: {e:#}", project.name);
                    errors.push(project.name.clone());
                }
            }
            errors
        }
        Command::Expiry { days } => {
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

            let targets: Vec<&config::Project> = match cli.project.as_deref() {
                Some(name) => vec![cfg.select_project(Some(name))?],
                None => cfg.projects.iter().collect(),
            };

            // 汇总全部项目/服务商的命中提醒，最后发一条通知（避免刷屏）
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut alerts: Vec<(String, String, ops::expiry::ExpiryAlert)> = Vec::new();
            let mut errors = Vec::new();
            for project in &targets {
                println!("\n===== 项目: {} =====", project.name);
                let kinds: Vec<&String> = match cli.provider.as_deref() {
                    Some(k) => {
                        if !project.providers.contains_key(k) {
                            anyhow::bail!(
                                "项目 {} 未配置服务商 {k:?}（可用: {}）",
                                project.name,
                                project.providers.keys().cloned().collect::<Vec<_>>().join(", ")
                            );
                        }
                        vec![project.providers.get_key_value(k).unwrap().0]
                    }
                    None => project.providers.keys().collect(),
                };
                for kind in kinds {
                    println!("-- 服务商: {kind}");
                    match run_provider_expiry(&cfg, project, kind, &thresholds).await {
                        Ok(list) => alerts.extend(
                            list.into_iter()
                                .map(|a| (project.name.clone(), kind.clone(), a)),
                        ),
                        Err(e) => {
                            println!("服务商 {kind} 检查失败: {e:#}");
                            errors.push(kind.clone());
                        }
                    }
                }
            }

            if !alerts.is_empty() {
                let text = ops::expiry::render(&alerts);
                println!("{text}");
                if let Some(n) = &notifier {
                    let title = format!("服务器到期提醒: {} 台需关注", alerts.len());
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
                println!("全部实例均在安全期内（{list} 天内无到期）");
            }

            if !errors.is_empty() {
                anyhow::bail!("以下服务商检查失败: {}", errors.join(", "));
            }
            Vec::new()
        }
        Command::Ecs { command } => match command {
            EcsCommand::AutoSnapshot => {
                run_ecs_autosnapshot(&cfg, cli.project.as_deref(), cli.provider.as_deref()).await?
            }
            EcsCommand::Expiry { days } => {
                run_ecs_expiry(&cfg, cli.project.as_deref(), cli.provider.as_deref(), &days).await?
            }
        },
    };

    if !project_errors.is_empty() {
        anyhow::bail!("以下项目执行失败: {}", project_errors.join(", "));
    }
    Ok(())
}

/// 对单个项目的全部（或 --provider 指定的）服务商执行快照轮转。
async fn run_project_rotate(
    cfg: &config::Config,
    project: &config::Project,
    provider_filter: Option<&str>,
    keep: usize,
    wait_minutes: u64,
) -> anyhow::Result<()> {
    let kinds: Vec<&String> = match provider_filter {
        Some(k) => {
            if !project.providers.contains_key(k) {
                anyhow::bail!("项目 {} 未配置服务商 {k:?}（可用: {}）", project.name,
                    project.providers.keys().cloned().collect::<Vec<_>>().join(", "));
            }
            vec![project.providers.get_key_value(k).unwrap().0]
        }
        None => project.providers.keys().collect(),
    };

    let mut errors = Vec::new();
    for kind in kinds {
        println!("-- 服务商: {kind}");
        if let Err(e) = run_provider_rotate(cfg, project, kind, keep, wait_minutes).await {
            println!("服务商 {kind} 执行失败: {e:#}");
            errors.push(kind.clone());
        }
    }
    if !errors.is_empty() {
        anyhow::bail!("服务商执行失败: {}", errors.join(", "));
    }
    Ok(())
}

/// 按服务商 kind 分发到具体实现（新服务商 = 在此加一个分支）。
async fn run_provider_rotate(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    keep: usize,
    wait_minutes: u64,
) -> anyhow::Result<()> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            rotate_provider(&provider, &cfg.notify, keep, wait_minutes).await
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
    }
}

/// 按服务商 kind 分发到期检查（新服务商 = 在此加一个分支）。
async fn run_provider_expiry(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    thresholds: &[i64],
) -> anyhow::Result<Vec<ops::expiry::ExpiryAlert>> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            ops::expiry::check(&provider, thresholds, chrono::Utc::now()).await
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
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

/// 项目内目标服务商 kind 列表（--provider 过滤 + allowed 白名单；默认 allowed 内的全部）
fn provider_kinds<'a>(
    project: &'a config::Project,
    filter: Option<&str>,
    allowed: &[&str],
) -> anyhow::Result<Vec<&'a String>> {
    match filter {
        Some(k) => {
            if !allowed.contains(&k) {
                anyhow::bail!(
                    "服务商 {k:?} 不支持该命令（可用: {}）",
                    allowed.join(", ")
                );
            }
            if !project.providers.contains_key(k) {
                anyhow::bail!(
                    "项目 {} 未配置服务商 {k:?}（可用: {}）",
                    project.name,
                    project.providers.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(vec![project.providers.get_key_value(k).unwrap().0])
        }
        None => Ok(project
            .providers
            .keys()
            .filter(|k| allowed.contains(&k.as_str()))
            .collect()),
    }
}

/// ECS 自动快照策略检查：遍历项目 × aliyun 配置，未开启的实例汇总通知。
async fn run_ecs_autosnapshot(
    cfg: &config::Config,
    project_filter: Option<&str>,
    provider_filter: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let targets = select_projects(cfg, project_filter)?;
    let notifier = crate::notify::from_config(&cfg.notify)?;
    let mut all: Vec<(String, String, ops::ecs::AutoSnapshotStatus)> = Vec::new();
    let mut errors = Vec::new();

    for project in &targets {
        println!("\n===== 项目: {} =====", project.name);
        let kinds = provider_kinds(project, provider_filter, &["aliyun"])?;
        if kinds.is_empty() {
            println!("  未配置 aliyun 服务商，跳过");
            continue;
        }
        for kind in kinds {
            println!("-- 服务商: {kind}");
            match run_provider_ecs_autosnapshot(cfg, project, kind).await {
                Ok(list) => all.extend(
                    list.into_iter()
                        .map(|s| (project.name.clone(), kind.clone(), s)),
                ),
                Err(e) => {
                    println!("服务商 {kind} 检查失败: {e:#}");
                    errors.push(kind.clone());
                }
            }
        }
    }

    if !all.is_empty() {
        println!("{}", ops::ecs::render_autosnapshot(&all));
        // 只通知未开启的实例
        let unprotected: Vec<_> = all
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

    if !errors.is_empty() {
        anyhow::bail!("以下服务商检查失败: {}", errors.join(", "));
    }
    Ok(errors)
}

/// ECS 到期提醒：遍历项目 × aliyun 配置，命中阈值（或已过期）汇总通知。
async fn run_ecs_expiry(
    cfg: &config::Config,
    project_filter: Option<&str>,
    provider_filter: Option<&str>,
    days: &str,
) -> anyhow::Result<Vec<String>> {
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

    let targets = select_projects(cfg, project_filter)?;
    let notifier = crate::notify::from_config(&cfg.notify)?;
    let mut alerts: Vec<(String, String, ops::expiry::ExpiryAlert)> = Vec::new();
    let mut errors = Vec::new();

    for project in &targets {
        println!("\n===== 项目: {} =====", project.name);
        let kinds = provider_kinds(project, provider_filter, &["aliyun"])?;
        if kinds.is_empty() {
            println!("  未配置 aliyun 服务商，跳过");
            continue;
        }
        for kind in kinds {
            println!("-- 服务商: {kind}");
            match run_provider_ecs_expiry(cfg, project, kind, &thresholds).await {
                Ok(list) => alerts.extend(
                    list.into_iter()
                        .map(|a| (project.name.clone(), kind.clone(), a)),
                ),
                Err(e) => {
                    println!("服务商 {kind} 检查失败: {e:#}");
                    errors.push(kind.clone());
                }
            }
        }
    }

    if !alerts.is_empty() {
        let text = ops::expiry::render(&alerts);
        println!("{text}");
        if let Some(n) = &notifier {
            let title = format!("ECS 到期提醒: {} 台需关注", alerts.len());
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
        println!("全部 ECS 实例均在安全期内（{list} 天内无到期）");
    }

    if !errors.is_empty() {
        anyhow::bail!("以下服务商检查失败: {}", errors.join(", "));
    }
    Ok(errors)
}

/// 单服务商自动快照检查（新服务商 = 在此加一个分支）。
async fn run_provider_ecs_autosnapshot(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
) -> anyhow::Result<Vec<ops::ecs::AutoSnapshotStatus>> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            ops::ecs::check_auto_snapshot(provider.ecs()).await
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
    }
}

/// 单服务商 ECS 到期检查（新服务商 = 在此加一个分支）。
async fn run_provider_ecs_expiry(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    thresholds: &[i64],
) -> anyhow::Result<Vec<ops::expiry::ExpiryAlert>> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            let servers = provider.ecs().list_servers().await?;
            Ok(ops::expiry::check_servers(servers, thresholds, chrono::Utc::now()))
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
    }
}

/// 对单服务商的全部实例执行轮转（只依赖 CloudProvider trait，与具体服务商无关）。
async fn rotate_provider<P: CloudProvider + ?Sized>(
    provider: &P,
    notify_cfg: &config::NotifyConfig,
    keep: usize,
    wait_minutes: u64,
) -> anyhow::Result<()> {
    let servers = provider.list_servers().await?;
    if servers.is_empty() {
        println!("  无实例，跳过");
        return Ok(());
    }

    let notifier = crate::notify::from_config(notify_cfg)?;
    let mut errors = Vec::new();
    for server in &servers {
        println!("  -- 实例: {} ({})", server.name, server.id);
        match ops::snapshot::rotate(
            provider,
            &server.id,
            keep,
            Duration::from_secs(wait_minutes * 60),
        )
        .await
        {
            Ok(summary) => {
                print!("{}", summary.render());
                // 通知渠道发送结果（失败不阻断主流程，避免告警本身导致退出码非零）
                if let Some(n) = &notifier {
                    let title = format!("快照轮转: {} 成功", summary.server_name);
                    if let Err(e) = n.send(&title, &summary.render()).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            }
            Err(e) => {
                println!("    实例 {} 轮转失败: {e:#}", server.name);
                errors.push(server.name.clone());
            }
        }
    }
    if !errors.is_empty() {
        anyhow::bail!("以下实例轮转失败: {}", errors.join(", "));
    }
    Ok(())
}
