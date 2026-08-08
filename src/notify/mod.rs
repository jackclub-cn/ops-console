//! 通知渠道抽象。
//!
//! 当前实现：钉钉自定义机器人（加签模式）。
//! 新渠道接入 = 实现 [`Notifier`] trait + 在 [`from_config`] 加一个分支
//! （如 Slack/Telegram/Webhook，参考 `cloud::CloudProvider` 的扩展方式）。

pub mod dingtalk;

use crate::config::NotifyConfig;
use anyhow::{anyhow, Result};

/// 通知渠道：运维事件（快照轮转成功/失败等）只依赖这个 trait 发通知，
/// 具体渠道差异被隔离在实现内部。
#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    /// 发送一条 markdown 通知。
    async fn send(&self, title: &str, text: &str) -> Result<()>;
}

/// 按配置构建通知器。
///
/// `kind = "none"`（或未配置/留空）时返回 `Ok(None)`，即不通知。
/// `kind = "dingtalk"` 但 webhook 为空时返回错误（fail-fast，避免静默不通知）。
pub fn from_config(cfg: &NotifyConfig) -> Result<Option<Box<dyn Notifier>>> {
    match cfg.kind.as_str() {
        "" | "none" => Ok(None),
        "dingtalk" => {
            if cfg.dingtalk.webhook.is_empty() {
                return Err(anyhow!(
                    "notify.kind = \"dingtalk\" 但 webhook 为空：请配置 [notify.dingtalk] webhook \
                     或环境变量 DINGTALK_WEBHOOK_URL"
                ));
            }
            Ok(Some(Box::new(dingtalk::DingTalkNotifier::new(
                &cfg.dingtalk.webhook,
                &cfg.dingtalk.secret,
                &cfg.prefix,
            ))))
        }
        other => Err(anyhow!("未知通知渠道: {other:?}（支持: none | dingtalk）")),
    }
}
