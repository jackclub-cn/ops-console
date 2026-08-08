//! 配置加载：config/providers.toml + 环境变量覆盖。

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::env;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub aliyun: AliyunConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AliyunConfig {
    #[serde(default = "default_region")]
    pub region: String,
    /// 留空则从环境变量 ALIYUN_ACCESS_KEY_ID 读取
    #[serde(default)]
    pub access_key_id: Option<String>,
    /// 留空则从环境变量 ALIYUN_ACCESS_KEY_SECRET 读取
    #[serde(default)]
    pub access_key_secret: Option<String>,
}

fn default_region() -> String {
    "cn-shenzhen".to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("读取配置文件失败 {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow!("解析配置文件失败 {}: {e}", path.display()))?;

        // 环境变量优先覆盖（CI / systemd / cron 场景）
        if let Ok(v) = env::var("ALIYUN_ACCESS_KEY_ID") {
            if !v.is_empty() {
                cfg.aliyun.access_key_id = Some(v);
            }
        }
        if let Ok(v) = env::var("ALIYUN_ACCESS_KEY_SECRET") {
            if !v.is_empty() {
                cfg.aliyun.access_key_secret = Some(v);
            }
        }
        Ok(cfg)
    }

    pub fn aliyun_credentials(&self) -> Result<(String, String)> {
        let id = self
            .aliyun
            .access_key_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeyId：请填写 config/providers.toml 或设置环境变量 ALIYUN_ACCESS_KEY_ID"
                )
            })?;
        let secret = self
            .aliyun
            .access_key_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeySecret：请填写 config/providers.toml 或设置环境变量 ALIYUN_ACCESS_KEY_SECRET"
                )
            })?;
        Ok((id, secret))
    }
}
