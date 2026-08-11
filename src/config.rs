//! 配置加载：config/project.yml（项目+服务商）+ config/notify.yml（通知渠道）+ 环境变量覆盖。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::path::Path;

/// 服务商级配置。字段名即 `kind`（如 `aliyun`），
/// 新服务商 = 在 provider 配置里加一节 `[project.providers.xxx]`。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ProviderConfig {
    #[serde(default = "default_region")]
    pub region: String,
    /// 留空则从环境变量 ALIYUN_ACCESS_KEY_ID / ALIYUN_ACCESS_KEY_SECRET 读取
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub access_key_secret: Option<String>,
}

/// region 特殊值：自动发现账号下全部地域（SWAS ListRegions ∪ ECS DescribeRegions）
pub const REGION_GLOBAL: &str = "global";

fn default_region() -> String {
    REGION_GLOBAL.to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// kind -> 服务商配置（如 `aliyun: { region: cn-shenzhen, ... }`）
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// 全部配置的解析结果。
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub projects: Vec<Project>,
    /// 通知渠道配置；缺省文件 = 不通知
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DingTalkConfig {
    /// 留空则从环境变量 DINGTALK_WEBHOOK_URL 读取
    #[serde(default)]
    pub webhook: String,
    /// 留空则从环境变量 DINGTALK_SECRET 读取
    #[serde(default)]
    pub secret: String,
}

impl Config {
    /// 从配置目录加载：`<dir>/project.yml` + `<dir>/notify.yml`（可选），随后应用环境变量覆盖。
    pub fn load(dir: &Path) -> Result<Self> {
        let projects_path = dir.join("project.yml");
        let text = std::fs::read_to_string(&projects_path)
            .map_err(|e| anyhow!("读取配置文件失败 {}: {e}", projects_path.display()))?;
        let notify_text = std::fs::read_to_string(dir.join("notify.yml")).ok();
        let mut cfg = Self::from_str(&text, notify_text.as_deref())?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// 纯解析（不读文件、不做环境变量覆盖）。用于配置校验与 Web 保存前验证。
    pub fn from_str(project_yml: &str, notify_yml: Option<&str>) -> Result<Self> {
        // YAML 文档根可以是数组，直接反序列化为项目列表
        let projects: Vec<Project> = serde_yaml::from_str(project_yml)
            .map_err(|e| anyhow!("解析项目配置失败: {e}"))?;
        if projects.is_empty() {
            return Err(anyhow!("没有配置任何项目（需要至少一个项目条目）"));
        }

        // notify.yml 可选：不存在 = 不通知
        let mut notify: NotifyConfig = match notify_yml {
            Some(t) => serde_yaml::from_str(t)
                .map_err(|e| anyhow!("解析通知配置失败: {e}"))?,
            None => NotifyConfig::default(),
        };
        if notify.kind.is_empty() && !notify.dingtalk.webhook.is_empty() {
            // 配置了 webhook 但没写 kind：视为 dingtalk，降低误配置成本
            notify.kind = "dingtalk".to_string();
        }

        Ok(Self { projects, notify })
    }

    /// 环境变量覆盖（CI / systemd / cron 场景；Web 保存配置时不会调用，避免覆盖文件值）
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = env::var("ALIYUN_ACCESS_KEY_ID") {
            if !v.is_empty() {
                for p in &mut self.projects {
                    if let Some(aliyun) = p.providers.get_mut("aliyun") {
                        aliyun.access_key_id = Some(v.clone());
                    }
                }
            }
        }
        if let Ok(v) = env::var("ALIYUN_ACCESS_KEY_SECRET") {
            if !v.is_empty() {
                for p in &mut self.projects {
                    if let Some(aliyun) = p.providers.get_mut("aliyun") {
                        aliyun.access_key_secret = Some(v.clone());
                    }
                }
            }
        }
        if let Ok(v) = env::var("DINGTALK_WEBHOOK_URL") {
            if !v.is_empty() {
                self.notify.dingtalk.webhook = v;
            }
        }
        if let Ok(v) = env::var("DINGTALK_SECRET") {
            if !v.is_empty() {
                self.notify.dingtalk.secret = v;
            }
        }
        // kind 推断：env 覆盖 webhook 之后补一次，保持旧 load 行为（纯 env 通知场景）
        if self.notify.kind.is_empty() && !self.notify.dingtalk.webhook.is_empty() {
            self.notify.kind = "dingtalk".to_string();
        }
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
                    "缺少阿里云 AccessKeyId：请填写 project.yml 或设置环境变量 ALIYUN_ACCESS_KEY_ID"
                )
            })?;
        let secret = self
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

/// 原子写文件：先写临时文件再 rename；Unix 下设置 0600。
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    // 随机后缀：避免并发保存（同进程多 handler）写同一 tmp 文件相互覆盖
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, content)
        .map_err(|e| anyhow!("写入临时文件失败 {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow!("设置权限失败 {}: {e}", tmp.display()))?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows rename 不能覆盖已存在文件：先删再试
            let _ = std::fs::remove_file(path);
            std::fs::rename(&tmp, path)
                .map_err(|e2| anyhow!("写文件失败 {}: {e} / {e2}", path.display()))
        }
    }
}

/// serve.yml：Web UI 访问令牌配置。token 为空时自动生成并写回。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ServeConfig {
    #[serde(default)]
    pub token: String,
}

impl ServeConfig {
    /// 读取 <dir>/serve.yml；不存在或 token 为空时生成随机 token 并保存。
    /// 返回 (配置, 本次是否新生成 token)。
    pub fn load_or_create(dir: &Path) -> Result<(Self, bool)> {
        let path = dir.join("serve.yml");
        let mut cfg: ServeConfig = match std::fs::read_to_string(&path) {
            Ok(t) => serde_yaml::from_str(&t)
                .map_err(|e| anyhow!("解析 {} 失败: {e}", path.display()))?,
            Err(_) => ServeConfig::default(),
        };
        if cfg.token.is_empty() {
            cfg.token = uuid::Uuid::new_v4().simple().to_string();
            write_atomic(&path, &serde_yaml::to_string(&cfg)?)?;
            return Ok((cfg, true));
        }
        Ok((cfg, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_str_ok() {
        let cfg = Config::from_str(
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n",
            None,
        )
        .unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].name, "demo");
    }

    #[test]
    fn test_region_default_global() {
        // region 不填 → 默认为 global（自动发现全部地域）
        let cfg = Config::from_str(
            "- name: demo\n  providers:\n    aliyun:\n      access_key_id: AKID\n",
            None,
        )
        .unwrap();
        assert_eq!(cfg.projects[0].providers["aliyun"].region, "global");
    }

    #[test]
    fn test_from_str_empty_rejected() {
        let err = Config::from_str("# 空文件\n", None).unwrap_err();
        assert!(err.to_string().contains("至少一个项目"));
    }

    #[test]
    fn test_from_str_bad_yaml_rejected() {
        assert!(Config::from_str(":: not yaml ::", None).is_err());
    }

    #[test]
    fn test_project_yaml_roundtrip() {
        let src = "- name: demo\n  description: 示例\n  providers:\n    aliyun:\n      region: cn-shenzhen\n      access_key_id: AKID\n";
        let cfg = Config::from_str(src, None).unwrap();
        let yaml = serde_yaml::to_string(&cfg.projects).unwrap();
        let back = Config::from_str(&yaml, None).unwrap();
        assert_eq!(back.projects[0].providers["aliyun"].access_key_id.as_deref(), Some("AKID"));
    }

    #[test]
    fn test_serve_config_create_and_reuse() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (first, first_generated) = ServeConfig::load_or_create(&dir).unwrap();
        assert!(!first.token.is_empty());
        let (second, second_generated) = ServeConfig::load_or_create(&dir).unwrap();
        assert!(first_generated, "首次调用应生成 token");
        assert!(!second_generated, "已存在的 serve.yml 不应再生成");
        assert_eq!(first.token, second.token, "已存在的 serve.yml 应复用 token");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_serve_config_empty_token_regenerated() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("serve.yml"), "token: \"\"\n").unwrap();
        let cfg = ServeConfig::load_or_create(&dir).unwrap().0;
        assert!(!cfg.token.is_empty());
        let saved: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("serve.yml")).unwrap()).unwrap();
        assert_eq!(saved["token"].as_str().unwrap(), cfg.token);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_env_webhook_infers_dingtalk_kind() {
        // notify.yml 缺失 + webhook 仅来自环境变量：load 后应推断 kind = dingtalk（保持旧行为）
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project.yml"),
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n",
        )
        .unwrap();

        let prev = std::env::var("DINGTALK_WEBHOOK_URL").ok();
        std::env::set_var(
            "DINGTALK_WEBHOOK_URL",
            "https://oapi.dingtalk.com/robot/send?access_token=test",
        );
        let cfg = Config::load(&dir).unwrap();
        match prev {
            Some(v) => std::env::set_var("DINGTALK_WEBHOOK_URL", v),
            None => std::env::remove_var("DINGTALK_WEBHOOK_URL"),
        }

        assert_eq!(
            cfg.notify.kind, "dingtalk",
            "仅 env 提供 webhook 时应推断 kind=dingtalk（纯 env 通知场景）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
