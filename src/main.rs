//! ops-console —— 服务商运维系统。
//!
//! 用法示例：
//!   ops-console projects
//!   ops-console snapshot --keep 2        # 快照轮转 + ECS 自动快照策略检查
//!   ops-console expiry --days 30,15,3    # SWAS + ECS 到期提醒
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
            let mut errors = Vec::new();
            for project in targets {
                println!("\n===== 项目: {} =====", project.name);
                if let Err(e) = run_project_rotate(&cfg, project, cli.provider.as_deref(), keep, wait_minutes)
                    .await
                {
                    println!("项目 {} 执行失败: {e:#}", project.name);
                    errors.push(project.name.clone());
                }
                // ECS 自动快照策略检查：巡检随快照轮转一起跑（未开启的实例汇总通知）
                if let Err(e) =
                    run_ecs_autosnapshot_project(&cfg, project, cli.provider.as_deref()).await
                {
                    println!("项目 {} ECS 自动快照检查失败: {e:#}", project.name);
                    errors.push(format!("{} (ECS 检查)", project.name));
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

            let targets = select_projects(&cfg, cli.project.as_deref())?;

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

                // ECS 到期检查：同账号凭据，kind 标记 aliyun-ecs 便于区分；
                // 仅当未指定 --provider 或指定 aliyun 时执行
                let do_ecs = match cli.provider.as_deref() {
                    Some(k) => k == "aliyun",
                    None => project.providers.contains_key("aliyun"),
                };
                if do_ecs {
                    println!("-- ECS 到期检查");
                    match run_provider_ecs_expiry(&cfg, project, "aliyun", &thresholds).await {
                        Ok(list) => alerts.extend(list.into_iter().map(|a| {
                            (project.name.clone(), "aliyun-ecs".to_string(), a)
                        })),
                        Err(e) => {
                            println!("ECS 到期检查失败: {e:#}");
                            errors.push("aliyun-ecs".to_string());
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
                // 轻量磁盘检查：仅当未指定 --provider 或指定 aliyun 时执行（与 ECS 门控一致）
                let do_swas = match cli.provider.as_deref() {
                    Some(k) => k == "aliyun",
                    None => project.providers.contains_key("aliyun"),
                };
                if do_swas {
                    match run_provider_disk_swas(&cfg, project, "aliyun", threshold).await {
                        Ok((o, m)) => {
                            over.extend(
                                o.into_iter()
                                    .map(|s| (project.name.clone(), "aliyun".into(), s)),
                            );
                            missing.extend(
                                m.into_iter()
                                    .map(|s| (project.name.clone(), "aliyun".into(), s)),
                            );
                        }
                        Err(e) => {
                            println!("服务商 aliyun 磁盘检查失败: {e:#}");
                            errors.push("aliyun".to_string());
                        }
                    }
                }
                // ECS 磁盘检查：同账号凭据，kind 标记 aliyun-ecs；
                // 仅当未指定 --provider 或指定 aliyun 时执行
                let do_ecs = match cli.provider.as_deref() {
                    Some(k) => k == "aliyun",
                    None => project.providers.contains_key("aliyun"),
                };
                if do_ecs {
                    match run_provider_disk_ecs(&cfg, project, "aliyun", threshold).await {
                        Ok((o, m)) => {
                            over.extend(
                                o.into_iter()
                                    .map(|s| (project.name.clone(), "aliyun-ecs".into(), s)),
                            );
                            missing.extend(
                                m.into_iter()
                                    .map(|s| (project.name.clone(), "aliyun-ecs".into(), s)),
                            );
                        }
                        Err(e) => {
                            println!("ECS 磁盘检查失败: {e:#}");
                            errors.push("aliyun-ecs".to_string());
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

/// 按服务商 kind 分发轻量磁盘检查（新服务商 = 在此加一个分支）。
async fn run_provider_disk_swas(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    threshold: f64,
) -> anyhow::Result<(Vec<ops::disk::DiskStatus>, Vec<ops::disk::DiskStatus>)> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            ops::disk::check_swas_disk(provider.swas(), threshold).await
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
    }
}

/// 按服务商 kind 分发 ECS 磁盘检查（新服务商 = 在此加一个分支）。
async fn run_provider_disk_ecs(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    threshold: f64,
) -> anyhow::Result<(Vec<ops::disk::DiskStatus>, Vec<ops::disk::DiskStatus>)> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            ops::disk::check_ecs_disk(provider.ecs(), provider.cms(), threshold).await
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

/// ECS 自动快照策略检查（单项目）：检查 aliyun 配置下的实例，未开启的汇总通知。
/// 由 `snapshot` 轮转时调用。
async fn run_ecs_autosnapshot_project(
    cfg: &config::Config,
    project: &config::Project,
    provider_filter: Option<&str>,
) -> anyhow::Result<()> {
    let kinds = provider_kinds(project, provider_filter, &["aliyun"])?;
    if kinds.is_empty() {
        println!("  未配置 aliyun 服务商，跳过 ECS 自动快照检查");
        return Ok(());
    }
    let notifier = crate::notify::from_config(&cfg.notify)?;
    let mut all: Vec<(String, String, ops::ecs::AutoSnapshotStatus)> = Vec::new();
    let mut errors = Vec::new();

    for kind in kinds {
        match run_provider_ecs_autosnapshot(cfg, project, kind).await {
            Ok(list) => all.extend(
                list.into_iter()
                    .map(|s| (project.name.clone(), kind.clone(), s)),
            ),
            Err(e) => {
                println!("服务商 {kind} ECS 检查失败: {e:#}");
                errors.push(kind.clone());
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
        anyhow::bail!("以下服务商 ECS 检查失败: {}", errors.join(", "));
    }
    Ok(())
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
mod serve;
