//! 阿里云 RPC 风格客户端公共封装：签名（sign.rs）+ HTTP + 统一错误处理 + 分页。
//!
//! 所有 RPC 风格产品（SWAS / ECS / SLB / DNS ...）共用，
//! 换 `product`（域名前缀）+ API Version + Action 即可。

use super::sign::sign_params;
use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;

#[derive(Debug, Clone)]
pub struct RpcClient {
    access_key_id: String,
    access_key_secret: String,
    region: String,
    /// 产品域名前缀（如 swas / ecs）
    product: String,
    http: reqwest::Client,
}

impl RpcClient {
    pub fn new(
        access_key_id: &str,
        access_key_secret: &str,
        region: &str,
        product: &str,
    ) -> Self {
        Self {
            access_key_id: access_key_id.to_string(),
            access_key_secret: access_key_secret.to_string(),
            region: region.to_string(),
            product: product.to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("构建 HTTP client 失败"),
        }
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// 通用 RPC 调用：签名 + GET + 统一错误处理（业务成功码："Success" 或空）
    pub async fn call<T: DeserializeOwned>(
        &self,
        action: &str,
        api_version: &str,
        extra: &[(&str, &str)],
    ) -> Result<T> {
        self.call_ok(action, api_version, extra, &[]).await
    }

    /// 同 [`call`]，但允许自定义业务成功码。
    /// 云监控（cms）成功响应返回 `Code: "200"`（而非 "Success"），需传 `&["200"]`。
    pub async fn call_ok<T: DeserializeOwned>(
        &self,
        action: &str,
        api_version: &str,
        extra: &[(&str, &str)],
        ok_codes: &[&str],
    ) -> Result<T> {
        let params = sign_params(
            &self.access_key_id,
            &self.access_key_secret,
            action,
            api_version,
            &self.region,
            extra,
        )?;

        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        // 全局服务（region 为空）：https://{product}.aliyuncs.com（如 domain），
        // 不拼地域段；区域化服务照常 {product}.{region}.aliyuncs.com
        let url = if self.region.is_empty() {
            format!("https://{}.aliyuncs.com/?{}", self.product, query)
        } else {
            format!(
                "https://{}.{}.aliyuncs.com/?{}",
                self.product, self.region, query
            )
        };

        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                // without_url：reqwest 错误 display 会包含完整请求 URL（含 AccessKeyId / Signature 等参数），
                // 去掉 URL 避免把凭据信息打进日志
                return Err(anyhow!(
                    "阿里云 {} API 请求失败 ({action}): {}",
                    self.product,
                    e.without_url()
                ));
            }
        };
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            return Err(anyhow!(
                "阿里云 {} API HTTP {} ({action}): {}",
                self.product,
                status.as_u16(),
                brief_http_error(&text)
            ));
        }

        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("响应解析失败 ({action}): {e} => {}", truncate(&text, 300)))?;

        if let Some(err) = business_error(&value, ok_codes) {
            return Err(anyhow!("阿里云 {} 业务错误 {err}", self.product));
        }

        serde_json::from_value(value).map_err(|e| anyhow!("响应反序列化失败 ({action}): {e}"))
    }

    /// 通用分页拉取：循环取到 TotalCount 为止。
    /// `extract` 从单页响应中取出 `(数据, TotalCount)`（不同产品的嵌套结构不同）。
    pub async fn paginate<T, E>(
        &self,
        action: &str,
        api_version: &str,
        extra: &[(&str, &str)],
        extract: impl Fn(E) -> (Vec<T>, i32),
    ) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
        E: DeserializeOwned,
    {
        const PAGE_SIZE: i32 = 100;
        let mut page = 1;
        let mut out = Vec::new();
        loop {
            let page_str = page.to_string();
            let size_str = PAGE_SIZE.to_string();
            let params: Vec<(&str, &str)> = extra
                .iter()
                .copied()
                .chain([
                    ("PageNumber", page_str.as_str()),
                    ("PageSize", size_str.as_str()),
                ])
                .collect();
            let resp: E = self.call(action, api_version, &params).await?;
            let (items, total) = extract(resp);
            out.extend(items);
            let fetched = out.len() as i32;
            if fetched >= total {
                break;
            }
            page += 1;
        }
        Ok(out)
    }
}

/// 错误响应文本 → 摘要：解析 JSON 取 Code/Message（截断 200 字符）。
/// 403/404 等响应里 EncodedDiagnosticMessage 等字段超长，直接截断原始文本会保留无价值噪音。
fn brief_http_error(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        let code = v.get("Code").and_then(|c| c.as_str()).unwrap_or_default();
        let msg = v.get("Message").and_then(|m| m.as_str()).unwrap_or_default();
        if !code.is_empty() || !msg.is_empty() {
            let mut s = String::new();
            if !code.is_empty() {
                s.push_str(code);
            }
            if !msg.is_empty() {
                if !s.is_empty() {
                    s.push_str(": ");
                }
                s.push_str(msg);
            }
            return truncate(&s, 200);
        }
    }
    truncate(text, 200)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}...")
    }
}

/// 检查响应 JSON 是否携带业务错误。
/// 返回 `Some(错误码[: 消息])` 表示失败；`None` 表示成功。
/// 业务成功判定：无 Code 字段 / Code 为空 / Code == "Success" / Code 在 ok_codes 中。
fn business_error(value: &serde_json::Value, ok_codes: &[&str]) -> Option<String> {
    let code = value.get("Code").and_then(|c| c.as_str())?;
    if code.is_empty() || code == "Success" || ok_codes.contains(&code) {
        return None;
    }
    let msg = value.get("Message").and_then(|m| m.as_str()).unwrap_or_default();
    if msg.is_empty() {
        Some(code.to_string())
    } else {
        Some(format!("{code}: {msg}"))
    }
}

/// 解析阿里云 ISO8601 到期时间（UTC）；空串/解析失败返回 None。
/// 兼容两种格式：
/// - SWAS：`2027-08-08T16:00:00+00:00`（标准 RFC3339）
/// - ECS 包年包月：`2027-11-28T16:00Z`（**缺秒**，parse_from_rfc3339 直接解析会失败）
pub fn parse_expired_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if s.is_empty() {
        return None;
    }
    parse_rfc3339_lenient(s).map(|t| t.with_timezone(&chrono::Utc))
}

/// 宽松 RFC3339 解析：标准格式优先，缺秒格式（`16:00Z` / `16:00+08:00`）补秒后重试。
fn parse_rfc3339_lenient(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t);
    }
    // 缺秒：从右往左找时区标记（Z / +HH:MM / -HH:MM），在时间后补 ":00"
    // 注意不能用 rsplit_once（它返回分隔符之后的内容，时区标记会被丢掉）
    let idx = s.rfind(['Z', '+', '-'])?;
    let (head, tail) = (&s[..idx], &s[idx..]);
    let head = head.trim_end_matches(':');
    let (_, mm) = head.rsplit_once(':')?;
    if mm.len() != 2 {
        return None;
    }
    let fixed = format!("{head}:00{tail}");
    chrono::DateTime::parse_from_rfc3339(&fixed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_expired_time() {
        use chrono::Utc;
        // SWAS 标准格式
        let t = parse_expired_time("2027-08-08T16:00:00+00:00").unwrap();
        assert_eq!(t, chrono::DateTime::parse_from_rfc3339("2027-08-08T16:00:00Z").unwrap().with_timezone(&Utc));
        // ECS 缺秒格式（核心回归：之前解析失败 → None）
        let t = parse_expired_time("2027-11-28T16:00Z").unwrap();
        assert_eq!(t, chrono::DateTime::parse_from_rfc3339("2027-11-28T16:00:00Z").unwrap().with_timezone(&Utc));
        // 缺秒 + 时区偏移
        let t = parse_expired_time("2027-11-28T16:00+08:00").unwrap();
        assert_eq!(t, chrono::DateTime::parse_from_rfc3339("2027-11-28T08:00:00Z").unwrap().with_timezone(&Utc));
        // 空/非法 → None
        assert!(parse_expired_time("").is_none());
        assert!(parse_expired_time("bad").is_none());
        assert!(parse_expired_time("2027-13-45T99:99Z").is_none());
    }

    #[test]
    fn test_brief_http_error() {
        // 403 响应：只取 Code/Message，丢弃 EncodedDiagnosticMessage 等超长字段
        let j = r#"{"Code":"NoPermission","Message":"User is not authorized.","EncodedDiagnosticMessage":"AQIBIAAA..."}"#;
        assert_eq!(brief_http_error(j), "NoPermission: User is not authorized.");
        // 无 JSON → 原样截断
        assert!(brief_http_error("plain text").contains("plain"));
    }

    #[test]
    fn test_business_error() {
        // Code=Success / 空 / 缺失 → 成功
        assert_eq!(business_error(&serde_json::json!({"Code": "Success"}), &[]), None);
        assert_eq!(business_error(&serde_json::json!({"Code": ""}), &[]), None);
        assert_eq!(business_error(&serde_json::json!({"Datapoints": "[]"}), &[]), None);

        // CMS 成功返回 Code="200"：默认判错误，传 ok_codes=["200"] 则成功
        let v = serde_json::json!({"Code": "200", "Message": "The specified resource is not found."});
        assert_eq!(
            business_error(&v, &[]),
            Some("200: The specified resource is not found.".to_string())
        );
        assert_eq!(business_error(&v, &["200"]), None);

        // 普通业务错误
        let v = serde_json::json!({"Code": "InvalidParameter", "Message": "bad param"});
        assert_eq!(
            business_error(&v, &[]),
            Some("InvalidParameter: bad param".to_string())
        );

        // Message 缺失 → 只返回错误码
        assert_eq!(business_error(&serde_json::json!({"Code": "Forbidden"}), &[]), Some("Forbidden".to_string()));
    }
}
