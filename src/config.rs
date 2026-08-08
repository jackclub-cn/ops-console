//! 配置加载：config/project.toml（项目+服务商）+ config/notify.toml（通知渠道）+ 环境变量覆盖。

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::Path;

/// 服务商级配置。字段名即 `kind`（如 `aliyun`），
/// 新服务商 = 在 provider 配置里加一节 `[project.providers.xxx]`。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default = "default_region")]
    pub region: String,
    /// 留空则从环境变量 ALIYUN_ACCESS_KEY_ID / ALIYUN_ACCESS_KEY_SECRET 读取
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub access_key_secret: Option<String>,
}

fn default_region() -> String {
    "cn-shenzhen".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// kind -> 服务商配置（如 `aliyun = { region = "cn-shenzhen", ... }`）
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// project.toml 文档根：`[[projects]]` 数组（TOML 文档根必须是表，不能直接反序列化为 Vec）。
#[derive(Debug, Clone, Deserialize)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<Project>,
}

/// 全部配置的解析结果。
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub projects: Vec<Project>,
    /// 通知渠道配置；缺省文件 = 不通知
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NotifyConfig {
    /// none | dingtalk（空 = 不通知）
    #[serde(default)]
    pub kind: String,
    /// 消息标题签名，如「【通知】」。钉钉自定义机器人无独立签名/别名字段，
    /// 通过前缀标记消息来源。空 = 不添加。
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub dingtalk: DingTalkConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DingTalkConfig {
    /// 留空则从环境变量 DINGTALK_WEBHOOK_URL 读取
    #[serde(default)]
    pub webhook: String,
    /// 留空则从环境变量 DINGTALK_SECRET 读取
    #[serde(default)]
    pub secret: String,
}

impl Config {
    /// 从配置目录加载：`<dir>/project.toml` + `<dir>/notify.toml`（可选）。
    pub fn load(dir: &Path) -> Result<Self> {
        let projects_path = dir.join("project.toml");
        let text = std::fs::read_to_string(&projects_path)
            .map_err(|e| anyhow!("读取配置文件失败 {}: {e}", projects_path.display()))?;
        let projects_file: ProjectsFile = toml::from_str(&text)
            .map_err(|e| anyhow!("解析配置文件失败 {}: {e}", projects_path.display()))?;
        let mut projects = projects_file.projects;
        if projects.is_empty() {
            return Err(anyhow!(
                "{} 中没有配置任何项目（需要至少一个 [[projects]] 条目）",
                projects_path.display()
            ));
        }

        // notify.toml 可选：不存在 = 不通知
        let notify_path = dir.join("notify.toml");
        let mut notify: NotifyConfig = match std::fs::read_to_string(&notify_path) {
            Ok(t) => toml::from_str(&t)
                .map_err(|e| anyhow!("解析配置文件失败 {}: {e}", notify_path.display()))?,
            Err(_) => NotifyConfig::default(),
        };

        // 环境变量覆盖（CI / systemd / cron 场景）
        if let Ok(v) = env::var("ALIYUN_ACCESS_KEY_ID") {
            if !v.is_empty() {
                for p in &mut projects {
                    if let Some(aliyun) = p.providers.get_mut("aliyun") {
                        aliyun.access_key_id = Some(v.clone());
                    }
                }
            }
        }
        if let Ok(v) = env::var("ALIYUN_ACCESS_KEY_SECRET") {
            if !v.is_empty() {
                for p in &mut projects {
                    if let Some(aliyun) = p.providers.get_mut("aliyun") {
                        aliyun.access_key_secret = Some(v.clone());
                    }
                }
            }
        }
        if let Ok(v) = env::var("DINGTALK_WEBHOOK_URL") {
            if !v.is_empty() {
                notify.dingtalk.webhook = v;
            }
        }
        if let Ok(v) = env::var("DINGTALK_SECRET") {
            if !v.is_empty() {
                notify.dingtalk.secret = v;
            }
        }
        if notify.kind.is_empty() && !notify.dingtalk.webhook.is_empty() {
            // 配置了 webhook 但没写 kind：视为 dingtalk，降低误配置成本
            notify.kind = "dingtalk".to_string();
        }

        Ok(Self { projects, notify })
    }

    /// 取项目下指定 kind 的服务商配置；未配置返回错误。
    pub fn provider<'a>(&self, project: &'a Project, kind: &str) -> Result<&'a ProviderConfig> {
        project.providers.get(kind).ok_or_else(|| {
            anyhow!(
                "项目 {} 未配置服务商 {kind:?}（可用: {}）",
                project.name,
                project
                    .providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }

    /// 按名称取项目；缺省取第一个。未找到返回错误。
    pub fn select_project(&self, name: Option<&str>) -> Result<&Project> {
        match name {
            Some(n) => self
                .projects
                .iter()
                .find(|p| p.name == n)
                .ok_or_else(|| {
                    let names: Vec<&str> = self.projects.iter().map(|p| p.name.as_str()).collect();
                    anyhow!("未找到项目 {n:?}（可用: {}）", names.join(", "))
                }),
            None => Ok(&self.projects[0]),
        }
    }
}

impl ProviderConfig {
    pub fn aliyun_credentials(&self) -> Result<(String, String)> {
        let id = self
            .access_key_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeyId：请填写 project.toml 或设置环境变量 ALIYUN_ACCESS_KEY_ID"
                )
            })?;
        let secret = self
            .access_key_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeySecret：请填写 project.toml 或设置环境变量 ALIYUN_ACCESS_KEY_SECRET"
                )
            })?;
        Ok((id, secret))
    }
}
