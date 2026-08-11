//! 阿里云资源中心（Resource Center）—— 跨地域资源目录。
//!
//! 产品文档：https://help.aliyun.com/zh/resource-management/resource-center/
//! endpoint: https://resourcecenter.aliyuncs.com（全局服务，不区分地域）
//! API Version: 2022-12-01
//! RAM 权限：`resourcecenter:SearchResources`（list 级别，全部资源）
//!
//! 用途：global 模式下的地域发现改为\"目录优先\"——SearchResources 返回账号下
//! 实际有实例的地域（ECS ∪ SWAS），只巡检这些地域，避免遍历全部地域
//! （32 个地域 × 每地域一次查询，含大量无权限/未开通地域的 403 噪音）。

use super::rpc::RpcClient;
use anyhow::Result;
use serde::Deserialize;

const RC_API_VERSION: &str = "2022-12-01";

/// 资源中心支持的资源类型常量（本文使用到的）
pub const TYPE_ECS_INSTANCE: &str = "ACS::ECS::Instance";
pub const TYPE_SWAS_INSTANCE: &str = "ACS::SWAS::Instance";

#[derive(Debug, Clone)]
pub struct ResourceCenterClient {
    rpc: RpcClient,
}

/// 目录条目（来自 SearchResources）。当前只消费地域信息（用于缩小巡检范围）。
#[derive(Debug, Clone)]
pub struct CatalogResource {
    pub region_id: String,
}

#[derive(Debug, Deserialize)]
struct SearchResourcesResponse {
    #[serde(rename = "Resources", default)]
    resources: Vec<SearchResource>,
    #[serde(rename = "NextToken", default)]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResource {
    #[serde(rename = "ResourceType", default)]
    resource_type: String,
    #[serde(rename = "RegionId", default)]
    region_id: String,
}

impl ResourceCenterClient {
    /// 构造客户端。资源中心是全局服务，region 留空 → https://resourcecenter.aliyuncs.com
    pub fn new(access_key_id: &str, access_key_secret: &str) -> Self {
        Self {
            rpc: RpcClient::new(access_key_id, access_key_secret, "", "resourcecenter"),
        }
    }

    /// 分页搜索指定资源类型的全部资源（MaxResults=100 + NextToken 翻页）。
    pub async fn search_resources(&self, resource_type: &str) -> Result<Vec<CatalogResource>> {
        let mut out = Vec::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut extra: Vec<(&str, &str)> =
                vec![("ResourceType", resource_type), ("MaxResults", "100")];
            if let Some(t) = &next_token {
                extra.push(("NextToken", t.as_str()));
            }
            let resp: SearchResourcesResponse = self
                .rpc
                .call("SearchResources", RC_API_VERSION, &extra)
                .await?;
            let count = resp.resources.len();
            // 注意：SearchResources 的 ResourceType 参数实际不生效（返回全部资源类型），
            // 且全局资源（如域名 ACS::Domain::Domain）的 RegionId 是 "global"，
            // 必须在客户端按 ResourceType 精确过滤，否则会把无关地域混入巡检范围。
            out.extend(resp.resources.into_iter().filter(|r| r.resource_type == resource_type).map(|r| CatalogResource {
                region_id: r.region_id,
            }));
            // 无 NextToken 或本页为空 → 结束（空页防死循环）
            match resp.next_token {
                Some(t) if !t.is_empty() && count > 0 => next_token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_search_resources_response() {
        // 单页含 NextToken 的响应解析
        let j = r#"{
            "Resources": [
                {"ResourceId":"i-bp1xxx","ResourceName":"web","ResourceType":"ACS::ECS::Instance","RegionId":"cn-shenzhen"},
                {"ResourceId":"d-bp1yyy","ResourceType":"ACS::ECS::Disk","RegionId":"cn-shenzhen"},
                {"ResourceId":"swas-2ze1xxx","ResourceType":"ACS::SWAS::Instance","RegionId":"cn-guangzhou"},
                {"ResourceId":"example.com","ResourceType":"ACS::Domain::Domain","RegionId":"global"}
            ],
            "NextToken":"eyJhbGciOi",
            "RequestId":"abc"
        }"#;
        let resp: SearchResourcesResponse = serde_json::from_str(j).unwrap();
        assert_eq!(resp.resources.len(), 4);
        // 客户端过滤语义：ECS 搜索只保留 ECS::Instance，排除 Disk 与 Domain（RegionId=global）
        let ecs: Vec<_> = resp.resources.iter().filter(|r| r.resource_type == TYPE_ECS_INSTANCE).collect();
        assert_eq!(ecs.len(), 1);
        assert_eq!(ecs[0].region_id, "cn-shenzhen");
        assert_eq!(resp.next_token.as_deref(), Some("eyJhbGciOi"));
    }
}
