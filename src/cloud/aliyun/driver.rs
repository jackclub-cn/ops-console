//! 阿里云驱动实现：命令分发 + 凭据解析。
//!
//! 命令方法（rotate / expiry / disk ...）在后续任务补齐，当前仅实现 kind / commands / credentials。

use crate::cloud::driver::{Command, ProviderDriver};
use crate::config::ProviderConfig;
use anyhow::{anyhow, Result};

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
