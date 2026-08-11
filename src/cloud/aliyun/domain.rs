//! 阿里云域名服务（Domain）API 封装 —— 域名到期提醒。
//!
//! 产品文档：https://help.aliyun.com/zh/dws/
//! endpoint: https://domain.aliyuncs.com（**全局服务，不区分地域**，勿拼 region 段；
//!            国际站为 domain-intl.aliyuncs.com）
//! API Version: 2018-01-29
//!
//! RAM 权限：查询类接口（QueryDomainList 等）对应 Action `domain:QueryCommonInfo`，
//! 授权粒度资源级；最小权限策略 `{"Action": "domain:QueryCommonInfo", "Resource": "*"}`
//! 或直接挂系统策略 AliyunDomainReadonlyAccess。

use super::rpc::RpcClient;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;

const DOMAIN_API_VERSION: &str = "2018-01-29";

#[derive(Debug, Clone)]
pub struct DomainClient {
    rpc: RpcClient,
}

#[derive(Debug, Deserialize)]
struct QueryDomainListResponse {
    #[serde(rename = "Data", default)]
    data: DomainData,
    #[serde(rename = "TotalItemNum", default)]
    total_item_num: i32,
}

#[derive(Debug, Deserialize, Default)]
struct DomainData {
    #[serde(rename = "Domain", default)]
    domains: Vec<DomainItem>,
}

#[derive(Debug, Deserialize, Clone)]
struct DomainItem {
    #[serde(rename = "DomainName")]
    domain_name: String,
    /// 到期时间，格式 `yyyy-mm-dd hh:mm:ss`（**北京时间**，无时区标记）
    #[serde(rename = "ExpirationDate", default)]
    expiration_date: String,
    #[serde(rename = "AutoRenewEnabled", default)]
    auto_renew_enabled: bool,
}

/// 统一域名模型（供到期提醒逻辑复用）
#[derive(Debug, Clone)]
pub struct DomainInfo {
    pub domain_name: String,
    /// 到期时间（UTC）；解析失败/缺失为 None（与服务器到期逻辑一致，跳过）
    pub expired_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
}

impl DomainClient {
    /// 构造域名客户端。域名是全局服务，region 留空 → 全局接入点 https://domain.aliyuncs.com
    pub fn new(access_key_id: &str, access_key_secret: &str) -> Self {
        Self {
            rpc: RpcClient::new(access_key_id, access_key_secret, "", "domain"),
        }
    }

    /// 分页拉取账号下全部域名（QueryDomainList，PageSize=100）。
    /// 注意：域名接口的分页参数名是 `PageNum`（非通用 RPC 分页的 `PageNumber`），
    /// 故不使用 RpcClient::paginate，自行循环。
    pub async fn list_domains(&self) -> Result<Vec<DomainInfo>> {
        let mut page = 1;
        let mut out = Vec::new();
        loop {
            let page_str = page.to_string();
            let resp: QueryDomainListResponse = self
                .rpc
                .call(
                    "QueryDomainList",
                    DOMAIN_API_VERSION,
                    &[
                        ("PageNum", page_str.as_str()),
                        ("PageSize", "100"),
                    ],
                )
                .await?;
            let fetched = resp.data.domains.len() as i32;
            out.extend(resp.data.domains.into_iter().map(|d| DomainInfo {
                domain_name: d.domain_name,
                expired_at: parse_domain_expiration(&d.expiration_date),
                auto_renew: d.auto_renew_enabled,
            }));
            // TotalItemNum 缺失或已取满 → 结束；防死循环兜底
            if out.len() as i32 >= resp.total_item_num || fetched == 0 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }
}

/// 解析域名到期时间：`yyyy-mm-dd hh:mm:ss`（北京时间）→ UTC。
/// 空串/非法返回 None。
fn parse_domain_expiration(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    // 追加时区偏移后按 RFC3339 风格解析，兼容各 chrono 版本
    let bjt = DateTime::parse_from_str(&format!("{s} +08:00"), "%Y-%m-%d %H:%M:%S %z").ok()?;
    Some(bjt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_domain_expiration() {
        // 北京时间 2017-11-02 04:00:45 → UTC 2017-11-01 20:00:45
        let t = parse_domain_expiration("2017-11-02 04:00:45").unwrap();
        assert_eq!(
            t,
            DateTime::parse_from_rfc3339("2017-11-01T20:00:45Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        // 空串 / 非法 → None
        assert!(parse_domain_expiration("").is_none());
        assert!(parse_domain_expiration("bad").is_none());
    }
}
