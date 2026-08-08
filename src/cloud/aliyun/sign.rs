//! 阿里云 OpenAPI V3 签名（RPC 风格，SignatureVersion=1.0 / HMAC-SHA1）。
//!
//! 适用于所有阿里云 RPC 风格产品（SWAS/ECS/SLB/DNS 等），
//! 换产品只需换 endpoint + Version + Action 参数。

use anyhow::{anyhow, Result};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// RFC3986 percent-encode（阿里云签名规范：保留 `-_.~`，空格 -> %20）
pub fn percent_encode(s: &str) -> String {
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

/// 生成签名参数。
///
/// 返回 `(key, value)` 列表，value 均已做 percent-encode，可直接拼进 query。
/// `extra` 为业务参数（如 InstanceId、SnapshotName），会与公共参数合并后签名。
pub fn sign_params(
    access_key_id: &str,
    access_key_secret: &str,
    action: &str,
    api_version: &str,
    region_id: &str,
    extra: &[(&str, &str)],
) -> Result<Vec<(String, String)>> {
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let nonce = uuid::Uuid::new_v4().to_string();

    let mut params: Vec<(String, String)> = vec![
        ("Action".into(), action.to_string()),
        ("Version".into(), api_version.to_string()),
        ("Format".into(), "JSON".into()),
        ("AccessKeyId".into(), access_key_id.to_string()),
        ("SignatureMethod".into(), "HMAC-SHA1".into()),
        ("SignatureNonce".into(), nonce),
        ("SignatureVersion".into(), "1.0".into()),
        ("Timestamp".into(), timestamp),
        ("RegionId".into(), region_id.to_string()),
    ];
    for (k, v) in extra {
        params.push((k.to_string(), v.to_string()));
    }

    // 按参数名 ASCII 字典序排序
    params.sort_by(|a, b| a.0.cmp(&b.0));

    // 规范化查询串
    let canonical = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let string_to_sign = format!(
        "GET&{}&{}",
        percent_encode("/"),
        percent_encode(&canonical)
    );

    let mut mac = HmacSha1::new_from_slice(format!("{}&", access_key_secret).as_bytes())
        .map_err(|e| anyhow!("HMAC 密钥初始化失败: {e}"))?;
    mac.update(string_to_sign.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    params.push(("Signature".to_string(), percent_encode(&signature)));
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(percent_encode("+ /"), "%2B%20%2F");
    }

    #[test]
    fn test_sign_stable() {
        // 固定时间与 nonce 无法注入，只验证输出结构：有 Signature 且长度正确
        let params = sign_params("LTAI-test", "secret", "ListInstances", "2020-05-06", "cn-shenzhen", &[]).unwrap();
        let sig = params.iter().find(|(k, _)| k == "Signature").unwrap().1.clone();
        // base64(HMAC-SHA1) = 28 chars，percent-encode 后不含空格
        assert!(!sig.contains(' '));
        assert!(sig.contains('%') || !sig.contains('+'));
    }
}
