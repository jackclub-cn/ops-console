//! ops-console —— 服务商运维系统。
//!
//! 用法示例：
//!   ops-console --project demo snapshot list --instance <id>
//!   ops-console snapshot rotate --instance <id> --keep 2
//!   ops-console --provider aliyun snapshot list --instance <id>
//!
//! 未指定 --project 时遍历全部项目；未指定 --provider 时执行项目内全部服务商。

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
