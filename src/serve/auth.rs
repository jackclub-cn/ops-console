//! 认证：token 解析（--token > env OPS_CONSOLE_TOKEN > serve.yml）+ Bearer/query 校验。

use crate::config;
use anyhow::{anyhow, Result};
use std::path::Path;

/// 解析最终访问令牌。env_getter 可注入（测试用），默认 std::env::var。
pub fn resolve_token(
    dir: &Path,
    override_token: Option<String>,
    env_getter: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    if let Some(t) = override_token.filter(|s| !s.is_empty()) {
        return Ok(t);
    }
    if let Some(t) = env_getter("OPS_CONSOLE_TOKEN").filter(|s| !s.is_empty()) {
        return Ok(t);
    }
    let serve = config::ServeConfig::load_or_create(dir)?;
    if serve.token.is_empty() {
        return Err(anyhow!("无法确定访问令牌（serve.yml 为空且无环境变量）"));
    }
    Ok(serve.token)
}

/// 校验器：比较请求携带的 token 与预期值（恒定时间比较，避免时序侧信道）。
#[derive(Clone)]
pub struct TokenValidator {
    expected: String,
}

impl TokenValidator {
    pub fn new(expected: &str) -> Self {
        Self { expected: expected.to_string() }
    }

    fn verify(&self, provided: Option<&str>) -> bool {
        match provided {
            Some(p) => {
                // 长度相同且逐字节比较
                p.len() == self.expected.len()
                    && p.bytes().zip(self.expected.bytes()).all(|(a, b)| a == b)
            }
            None => false,
        }
    }

    pub fn verify_header(&self, header: &str) -> bool {
        self.verify(header.strip_prefix("Bearer "))
    }

    pub fn verify_query(&self, token: &str) -> bool {
        self.verify(Some(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_resolution_priority() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // serve.yml 有 token
        std::fs::write(dir.join("serve.yml"), "token: from-file\n").unwrap();
        // 无 override、无 env → 文件值
        assert_eq!(resolve_token(&dir, None, |_| None).unwrap(), "from-file");
        // override 优先于 env
        assert_eq!(
            resolve_token(&dir, Some("from-arg".into()), |_| Some("from-env".into())).unwrap(),
            "from-arg"
        );
        // 无 override、有 env → env 优先于文件
        assert_eq!(
            resolve_token(&dir, None, |_| Some("from-env".into())).unwrap(),
            "from-env"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_verify_ok_and_bad() {
        let v = TokenValidator::new("secret-token");
        assert!(v.verify_header("Bearer secret-token"));
        assert!(!v.verify_header("Bearer wrong"));
        assert!(!v.verify_header(""));
        assert!(!v.verify_query("wrong"));
        assert!(v.verify_query("secret-token"));
    }
}
