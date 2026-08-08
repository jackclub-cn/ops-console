//! 钉钉自定义机器人通知（加签模式）。
//!
//! 文档: https://open.dingtalk.com/document/orgapp/custom-robots-send-group-messages
//! 安全设置: https://open.dingtalk.com/document/orgapp/security-settings

use crate::notify::Notifier;
use anyhow::{anyhow, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// 钉钉自定义机器人（markdown 消息 + 加签）。
pub struct DingTalkNotifier {
    webhook: String,
    secret: String,
    /// 标题签名（如「【通知】」），空 = 不添加
    prefix: String,
    http: reqwest::Client,
}

impl DingTalkNotifier {
    pub fn new(webhook: &str, secret: &str, prefix: &str) -> Self {
        Self {
            webhook: webhook.to_string(),
            secret: secret.to_string(),
            prefix: prefix.to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(15))
                .build()
                .expect("构建 HTTP client 失败"),
        }
    }

    /// 加签：`HMAC-SHA256("{ts}\n{secret}")` → base64 → URL 编码，拼到 webhook query。
    fn sign(secret: &str, ts_ms: i64) -> String {
        let string_to_sign = format!("{ts_ms}\n{secret}");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC 密钥初始化失败");
        mac.update(string_to_sign.as_bytes());
        let digest =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        percent_encode(&digest)
    }
}

/// 构造钉钉 markdown 消息体（纯函数，便于测试）。
/// 标题签名：群列表标题 + 正文加粗标题均带前缀，来源一眼可辨。
fn build_markdown(prefix: &str, title: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "msgtype": "markdown",
        "markdown": {
            "title": format!("{prefix}{title}"),
            "text": format!("**{prefix}{title}**\n\n{text}"),
        },
    })
}

#[async_trait::async_trait]
impl Notifier for DingTalkNotifier {
    async fn send(&self, title: &str, text: &str) -> Result<()> {
        let ts = chrono::Utc::now().timestamp_millis();
        let sign = Self::sign(&self.secret, ts);
        let url = format!("{}&timestamp={ts}&sign={sign}", self.webhook);

        let body = build_markdown(&self.prefix, title, text);

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        let resp_text = resp.text().await?;

        if !status.is_success() {
            return Err(anyhow!(
                "钉钉 webhook HTTP {}: {}",
                status.as_u16(),
                resp_text.chars().take(300).collect::<String>()
            ));
        }

        // 业务码：errcode == 0 为成功（HTTP 200 也可能带业务错误）
        let value: serde_json::Value =
            serde_json::from_str(&resp_text).unwrap_or(serde_json::Value::Null);
        if let Some(code) = value.get("errcode").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = value
                    .get("errmsg")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                return Err(anyhow!("钉钉返回错误 errcode={code}: {msg}"));
            }
        }
        Ok(())
    }
}

/// RFC3986 percent-encode（保留 `-_.~`）。base64 签名的字符集（`+`/`/`/`=`）编码结果
/// 与钉钉文档要求的 urlEncode（quote_plus）一致。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_matches_known_vector() {
        // 参考值用 Python 独立算出：urllib.parse.quote_plus(base64(hmac_sha256("1700000000000\nSEC0123456789abcdef", "SEC0123456789abcdef")))
        let sign = DingTalkNotifier::sign("SEC0123456789abcdef", 1700000000000);
        assert_eq!(
            sign,
            "TSZbRFUuvaSQaRKUpF970OPCb2%2FLcQAP3wOvwZIzBZk%3D"
        );
    }

    #[test]
    fn test_build_markdown_with_prefix() {
        let body = build_markdown("【通知】", "快照轮转: jackclub 成功", "  删除: 无\n  当前快照总数: 2\n");
        assert_eq!(body["markdown"]["title"], "【通知】快照轮转: jackclub 成功");
        assert_eq!(
            body["markdown"]["text"],
            "**【通知】快照轮转: jackclub 成功**\n\n  删除: 无\n  当前快照总数: 2\n"
        );
    }

    #[test]
    fn test_build_markdown_without_prefix() {
        let body = build_markdown("", "轮转成功", "详情");
        assert_eq!(body["markdown"]["title"], "轮转成功");
        assert_eq!(body["markdown"]["text"], "**轮转成功**\n\n详情");
    }
}
