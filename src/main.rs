//! ops-console —— 服务商运维系统。
//!
//! 用法示例：
//!   ops-console aliyun instance list
//!   ops-console aliyun snapshot list --instance <id>
//!   ops-console aliyun snapshot rotate --instance <id> --keep 2

mod cloud;
mod config;
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
    /// 配置文件路径
    #[arg(long, default_value = "config/providers.toml")]
    config: String,

    /// 日志级别 (error|warn|info|debug)
    #[arg(long, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 阿里云
    #[command(subcommand)]
    Aliyun(AliyunCmd),
}

#[derive(Subcommand)]
enum AliyunCmd {
    /// 实例操作
    #[command(subcommand)]
    Instance(InstanceCmd),

    /// 快照操作
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
}

#[derive(Subcommand)]
enum InstanceCmd {
    /// 列出实例
    List {
        /// 按名称过滤（模糊匹配）
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// 列出实例快照
    List {
        #[arg(long)]
        instance: String,
    },

    /// 手动创建快照
    Create {
        #[arg(long)]
        instance: String,

        /// 快照名（默认 snap-<时间戳>）
        #[arg(long)]
        name: Option<String>,
    },

    /// 轮转：删旧建新，保留 keep 份可用快照
    Rotate {
        #[arg(long)]
        instance: String,

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

    match cli.command {
        Command::Aliyun(aliyun_cmd) => {
            let (ak, sk) = cfg.aliyun_credentials()?;
            let provider =
                cloud::aliyun::AliyunProvider::new(&ak, &sk, &cfg.aliyun.region);

            match aliyun_cmd {
                AliyunCmd::Instance(InstanceCmd::List { name }) => {
                    let servers = provider.list_servers().await?;
                    println!(
                        "{:<24} {:<32} {:<14} {}",
                        "INSTANCE_ID", "NAME", "REGION", "STATUS"
                    );
                    for s in servers {
                        if let Some(n) = &name {
                            if !s.name.contains(n.as_str()) {
                                continue;
                            }
                        }
                        println!("{:<24} {:<32} {:<14} {}", s.id, s.name, s.region, s.status);
                    }
                }
                AliyunCmd::Snapshot(snapshot_cmd) => match snapshot_cmd {
                    SnapshotCmd::List { instance } => {
                        let snaps = provider.list_snapshots(&instance).await?;
                        if snaps.is_empty() {
                            println!("实例 {instance} 暂无快照");
                        } else {
                            println!(
                                "{:<26} {:<36} {:<10} {}",
                                "SNAPSHOT_ID", "NAME", "STATUS", "CREATED_AT"
                            );
                            for s in snaps {
                                println!(
                                    "{:<26} {:<36} {:<10} {}",
                                    s.id,
                                    s.name,
                                    s.status.as_str(),
                                    s.created_at.unwrap_or_default()
                                );
                            }
                        }
                    }
                    SnapshotCmd::Create { instance, name } => {
                        let name = name.unwrap_or_else(|| {
                            ops::snapshot::default_snapshot_name("snap")
                        });
                        let id = provider.create_snapshot(&instance, &name).await?;
                        println!("已创建快照: {id} ({name})");
                    }
                    SnapshotCmd::Rotate {
                        instance,
                        keep,
                        wait_minutes,
                    } => {
                        let summary = ops::snapshot::rotate(
                            &provider,
                            &instance,
                            keep,
                            Duration::from_secs(wait_minutes * 60),
                        )
                        .await?;
                        print!("{}", summary.render());
                    }
                },
            }
        }
    }
    Ok(())
}
