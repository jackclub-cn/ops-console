//! 服务商驱动注册表：命令分发 + 凭据解析的唯一入口。
//!
//! 接入新服务商 = 实现 [`ProviderDriver`] + 在 [`drivers`] 注册一行；
//! [`supported_kinds`] 与命令分发均从注册表派生，无需再同步其它位置。

use crate::config::{Config, Project, ProviderConfig};
use crate::ops::disk::DiskStatus;
use crate::ops::ecs::AutoSnapshotStatus;
use crate::ops::expiry::{DomainAlert, ExpiryAlert};
use anyhow::{anyhow, Result};

/// 服务商可执行的命令类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 快照轮转
    Snapshot,
    /// 服务器到期提醒
    Expiry,
    /// 域名到期提醒
    ExpiryDomain,
    /// 磁盘占用检查
    Disk,
    /// ECS 自动快照策略检查（随 snapshot 执行）
    EcsAutosnapshot,
    /// ECS 到期提醒
    EcsExpiry,
}

/// 磁盘检查的一个资源族结果（label 由驱动决定，如 aliyun 的 "aliyun" / "aliyun-ecs"）。
#[derive(Debug, Default)]
pub struct DiskGroup {
    pub label: String,
    pub over: Vec<DiskStatus>,
    pub missing: Vec<DiskStatus>,
    /// 该资源族全部地域失败时 Some（错误信息）；成功时 None
    pub error: Option<String>,
}

/// 磁盘检查全部结果（可能多个资源族）。
#[derive(Debug, Default)]
pub struct DiskGroups {
    pub groups: Vec<DiskGroup>,
}

/// 服务商驱动：一个服务商的一种接入实现。
#[async_trait::async_trait]
pub trait ProviderDriver: Send + Sync {
    /// 唯一 kind（与 project.yml 中 `providers.<kind>` 键一致）。
    fn kind(&self) -> &'static str;

    /// 支持的命令列表（默认不支持任何命令；逐项覆盖）。
    /// 未声明的命令 main 层不会调用对应方法。
    fn commands(&self) -> &[Command] {
        &[]
    }

    /// 解析项目下该服务商的凭据。
    /// 默认实现读取通用字段 `access_key_id` / `access_key_secret`（环境变量已在配置加载时注入）。
    fn credentials(&self, pcfg: &ProviderConfig) -> Result<(String, String)> {
        let id = pcfg
            .access_key_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少 {} 凭据 AccessKeyId：请填写 project.yml 或设置环境变量",
                    self.kind()
                )
            })?;
        let secret = pcfg
            .access_key_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少 {} 凭据 AccessKeySecret：请填写 project.yml 或设置环境变量",
                    self.kind()
                )
            })?;
        Ok((id, secret))
    }

    /// 快照轮转。
    async fn rotate(
        &self,
        _cfg: &Config,
        _project: &Project,
        _keep: usize,
        _wait_minutes: u64,
    ) -> Result<()> {
        anyhow::bail!("服务商 {} 不支持快照轮转", self.kind())
    }

    /// 服务器到期提醒（主资源）。
    async fn expiry(
        &self,
        _cfg: &Config,
        _project: &Project,
        _thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        anyhow::bail!("服务商 {} 不支持到期提醒", self.kind())
    }

    /// 域名到期提醒（账号级全局资源）。
    async fn domain_expiry(
        &self,
        _cfg: &Config,
        _project: &Project,
        _thresholds: &[i64],
    ) -> Result<Vec<DomainAlert>> {
        anyhow::bail!("服务商 {} 不支持域名到期提醒", self.kind())
    }

    /// 磁盘占用检查（返回按资源族分组）。
    async fn disk(
        &self,
        _cfg: &Config,
        _project: &Project,
        _threshold: f64,
    ) -> Result<DiskGroups> {
        anyhow::bail!("服务商 {} 不支持磁盘检查", self.kind())
    }

    /// ECS 自动快照策略检查。
    async fn ecs_autosnapshot(
        &self,
        _cfg: &Config,
        _project: &Project,
    ) -> Result<Vec<AutoSnapshotStatus>> {
        anyhow::bail!("服务商 {} 不支持自动快照检查", self.kind())
    }

    /// ECS 到期提醒。
    async fn ecs_expiry(
        &self,
        _cfg: &Config,
        _project: &Project,
        _thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        anyhow::bail!("服务商 {} 不支持 ECS 到期", self.kind())
    }

    /// 资源快照（Web 资源列表）：返回 `{ <resource_kind>: {...} }` JSON。
    async fn resources(
        &self,
        _cfg: &Config,
        _project: &Project,
    ) -> Result<serde_json::Value> {
        anyhow::bail!("服务商 {} 不支持资源列表", self.kind())
    }
}

/// 全部已注册驱动（唯一权威来源；`supported_kinds` 由此派生）。
/// 接入新服务商 = 实现 `ProviderDriver` + 在此追加一行。
static ALIYUN_DRIVER: crate::cloud::aliyun::AliyunDriver = crate::cloud::aliyun::AliyunDriver;
static DRIVERS: &[&'static dyn ProviderDriver] = &[&ALIYUN_DRIVER];

pub fn drivers() -> &'static [&'static dyn ProviderDriver] {
    DRIVERS
}

/// 按 kind 查驱动。
pub fn driver(kind: &str) -> Option<&'static dyn ProviderDriver> {
    drivers().iter().find(|d| d.kind() == kind).copied()
}

/// 支持的服务商 kind 列表（前端下拉、`--provider` 校验统一用）。
pub fn supported_kinds() -> Vec<&'static str> {
    drivers().iter().map(|d| d.kind()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_lookup() {
        assert!(driver("aliyun").is_some());
        assert!(driver("tencent").is_none());
    }

    #[test]
    fn test_supported_kinds() {
        assert_eq!(supported_kinds(), vec!["aliyun"]);
    }

    #[test]
    fn test_drivers_unique_kinds() {
        let mut kinds: Vec<&str> = drivers().iter().map(|d| d.kind()).collect();
        kinds.sort_unstable();
        let mut dedup = kinds.clone();
        dedup.dedup();
        assert_eq!(kinds, dedup, "驱动 kind 必须唯一");
    }
}
