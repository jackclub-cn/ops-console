//! 阿里云驱动实现：命令分发 + 凭据解析。
//!
//! 命令方法（rotate / expiry / disk ...）在后续任务补齐，当前仅实现 kind / commands / credentials。

use crate::cloud::aliyun::{scan_regions, AliyunProvider};
use crate::cloud::driver::{Command, DiskGroup, DiskGroups, ProviderDriver};
use crate::config::{Config, Project, ProviderConfig};
use crate::ops;
use crate::ops::ecs::AutoSnapshotStatus;
use crate::ops::expiry::{DomainAlert, ExpiryAlert};
use anyhow::{anyhow, Result};
use chrono::Utc;

/// 阿里云服务商驱动（SWAS 轻量 + ECS + 域名）。
pub struct AliyunDriver;

#[async_trait::async_trait]
impl ProviderDriver for AliyunDriver {
    fn kind(&self) -> &'static str {
        "aliyun"
    }

    fn commands(&self) -> &[Command] {
        &[
            Command::Snapshot,
            Command::Expiry,
            Command::ExpiryDomain,
            Command::Disk,
            Command::EcsAutosnapshot,
            Command::EcsExpiry,
        ]
    }

    fn credentials(&self, pcfg: &ProviderConfig) -> Result<(String, String)> {
        // 环境变量（ALIYUN_ACCESS_KEY_ID / ALIYUN_ACCESS_KEY_SECRET）已在 Config::load 时注入，
        // 此处只读字段并报错。错误消息保留旧 aliyun_credentials 的语义。
        let id = pcfg
            .access_key_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeyId：请填写 project.yml 或设置环境变量 ALIYUN_ACCESS_KEY_ID"
                )
            })?;
        let secret = pcfg
            .access_key_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeySecret：请填写 project.yml 或设置环境变量 ALIYUN_ACCESS_KEY_SECRET"
                )
            })?;
        Ok((id, secret))
    }

    async fn rotate(
        &self,
        cfg: &Config,
        project: &Project,
        keep: usize,
        wait_minutes: u64,
    ) -> Result<()> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        ops::snapshot::rotate_provider(&provider, &cfg.notify, keep, wait_minutes).await
    }

    async fn expiry(
        &self,
        cfg: &Config,
        project: &Project,
        thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        ops::expiry::check(&provider, thresholds, Utc::now()).await
    }

    async fn domain_expiry(
        &self,
        cfg: &Config,
        project: &Project,
        thresholds: &[i64],
    ) -> Result<Vec<DomainAlert>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let client = crate::cloud::aliyun::domain::DomainClient::new(&ak, &sk);
        ops::expiry::check_domains(&client, thresholds, Utc::now()).await
    }

    async fn disk(
        &self,
        cfg: &Config,
        project: &Project,
        threshold: f64,
    ) -> Result<DiskGroups> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        let mut groups = Vec::new();

        // SWAS → label "aliyun"
        let scan = scan_regions(provider.groups(), "SWAS 磁盘检查", "SWAS", |g| async move {
            ops::disk::check_swas_disk(&g.swas, &g.region, threshold).await
        })
        .await;
        let err = scan.all_failed_err("SWAS 磁盘检查");
        let (mut over, mut missing) = (Vec::new(), Vec::new());
        for (o, m) in scan.items {
            over.extend(o);
            missing.extend(m);
        }
        groups.push(DiskGroup {
            label: "aliyun".to_string(),
            over,
            missing,
            error: err.map(|e| format!("{e:#}")),
        });

        // ECS → label "aliyun-ecs"
        let scan = scan_regions(provider.groups(), "ECS 磁盘检查", "ECS", |g| async move {
            ops::disk::check_ecs_disk(&g.ecs, &g.cms, threshold).await
        })
        .await;
        let err = scan.all_failed_err("ECS 磁盘检查");
        let (mut over, mut missing) = (Vec::new(), Vec::new());
        for (o, m) in scan.items {
            over.extend(o);
            missing.extend(m);
        }
        groups.push(DiskGroup {
            label: "aliyun-ecs".to_string(),
            over,
            missing,
            error: err.map(|e| format!("{e:#}")),
        });

        Ok(DiskGroups { groups })
    }

    async fn ecs_autosnapshot(
        &self,
        cfg: &Config,
        project: &Project,
    ) -> Result<Vec<AutoSnapshotStatus>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        let scan = scan_regions(provider.groups(), "ECS 自动快照检查", "ECS", |g| async move {
            ops::ecs::check_auto_snapshot(&g.ecs).await
        })
        .await;
        if let Some(e) = scan.all_failed_err("ECS 自动快照检查") {
            return Err(e);
        }
        Ok(scan.items.into_iter().flatten().collect())
    }

    async fn ecs_expiry(
        &self,
        cfg: &Config,
        project: &Project,
        thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        let scan = scan_regions(provider.groups(), "ECS 查询", "ECS", |g| async move {
            g.ecs.list_servers().await
        })
        .await;
        if let Some(e) = scan.all_failed_err("ECS 查询") {
            return Err(e);
        }
        let servers: Vec<_> = scan.items.into_iter().flatten().collect();
        Ok(ops::expiry::check_servers(servers, thresholds, Utc::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_and_commands() {
        let d = AliyunDriver;
        assert_eq!(d.kind(), "aliyun");
        assert!(d.commands().contains(&Command::Snapshot));
        assert!(d.commands().contains(&Command::EcsExpiry));
        assert_eq!(d.commands().len(), 6);
    }

    #[test]
    fn test_credentials_missing() {
        let pcfg = ProviderConfig::default();
        let err = AliyunDriver.credentials(&pcfg).unwrap_err();
        assert!(err.to_string().contains("AccessKeyId"));
    }

    #[test]
    fn test_credentials_ok() {
        let pcfg = ProviderConfig {
            region: "cn-shenzhen".into(),
            access_key_id: Some("AKID".into()),
            access_key_secret: Some("SECRET".into()),
        };
        let (id, secret) = AliyunDriver.credentials(&pcfg).unwrap();
        assert_eq!(id, "AKID");
        assert_eq!(secret, "SECRET");
    }
}
