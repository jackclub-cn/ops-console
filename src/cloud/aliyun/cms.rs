//! 阿里云云监控（CMS）API 封装 —— ECS 磁盘使用率等操作系统监控指标。
//!
//! endpoint: https://metrics.{region}.aliyuncs.com
//! API Version: 2019-01-01
//! 注意：CMS 业务成功时返回 `Code: "200"`（而非 "Success"），须用 `call_ok(..., &["200"])`。

use super::rpc::RpcClient;
use anyhow::Result;
use serde::Deserialize;

const CMS_API_VERSION: &str = "2019-01-01";

/// 一条监控数据点（已从 Datapoints JSON 字符串解析）
#[derive(Debug, Clone)]
pub struct MetricPoint {
    pub instance_id: String,
    /// 磁盘设备（如 /dev/vda1）；无设备维度的指标为空
    pub device: String,
    /// 统计值（Average）
    pub average: f64,
}

#[derive(Debug, Deserialize)]
struct DescribeMetricLastResponse {
    /// Datapoints 是 JSON 编码的字符串数组
    #[serde(rename = "Datapoints", default)]
    datapoints: String,
}

#[derive(Debug, Clone)]
pub struct CmsClient {
    rpc: RpcClient,
}

impl CmsClient {
    pub fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Self {
        Self {
            rpc: RpcClient::new(access_key_id, access_key_secret, region, "metrics"),
        }
    }

    /// 查询某指标的最新数据点。
    /// `dims` 为 JSON 字符串（如 `{"instanceId":"i-xxx"}`）；None = 查询账号全部实例。
    pub async fn describe_metric_last(
        &self,
        namespace: &str,
        metric: &str,
        dims: Option<&str>,
    ) -> Result<Vec<MetricPoint>> {
        let mut extra: Vec<(&str, &str)> = vec![("Namespace", namespace), ("MetricName", metric)];
        if let Some(d) = dims {
            extra.push(("Dimensions", d));
        }
        let resp: DescribeMetricLastResponse = self
            .rpc
            .call_ok("DescribeMetricLast", CMS_API_VERSION, &extra, &["200"])
            .await?;
        Ok(parse_points(&resp.datapoints))
    }
}

/// 解析 DescribeMetricLast 的 Datapoints（JSON 数组字符串）为数据点列表。
/// 元素字段：小写 `timestamp`/`userId`/`instanceId`，大写 `Minimum`/`Average`/`Maximum`，磁盘指标带 `device`。
/// 兼容大小写变体；缺 instanceId 或缺数值（Average 链取不到）的点跳过，不默认为 0；非法输入返回空列表。
fn parse_points(datapoints: &str) -> Vec<MetricPoint> {
    let arr: Vec<serde_json::Value> = match serde_json::from_str(datapoints) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for p in arr {
        let instance_id = p
            .get("instanceId")
            .or_else(|| p.get("InstanceId"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if instance_id.is_empty() {
            continue;
        }
        let device = p
            .get("device")
            .or_else(|| p.get("Device"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let Some(average) = p
            .get("Average")
            .or_else(|| p.get("average"))
            .or_else(|| p.get("Maximum"))
            .and_then(|v| v.as_f64())
        else {
            // 缺数值：跳过该点，避免下游把缺值误读为 0% 使用率
            continue;
        };
        out.push(MetricPoint {
            instance_id: instance_id.to_string(),
            device: device.to_string(),
            average,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_points() {
        // 官方文档样例：小写 instanceId、大写 Average/Maximum、无 device
        let s = r#"[{"timestamp":1548777660000,"userId":"123456789876****","instanceId":"i-abcdefgh12****","Minimum":93.1,"Average":99.52,"Maximum":100}]"#;
        let pts = parse_points(s);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].instance_id, "i-abcdefgh12****");
        assert_eq!(pts[0].device, "");
        assert_eq!(pts[0].average, 99.52);

        // 磁盘指标带 device 维度
        let s = r#"[{"timestamp":1699219200000,"instanceId":"i-bp1xxx","device":"/dev/vda1","Average":91.2,"Maximum":93.0},{"timestamp":1699219200000,"instanceId":"i-bp1xxx","device":"/dev/vdb1","Average":30.1}]"#;
        let pts = parse_points(s);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].device, "/dev/vda1");
        assert_eq!(pts[0].average, 91.2);
        assert_eq!(pts[1].device, "/dev/vdb1");

        // 空数组 / 空串 / 非 JSON → 空
        assert!(parse_points("[]").is_empty());
        assert!(parse_points("").is_empty());
        assert!(parse_points("bad").is_empty());
        // 缺 instanceId → 跳过该点
        assert!(parse_points(r#"[{"Average": 50.0}]"#).is_empty());
        // 缺数值（Average 链取不到）→ 跳过该点，不默认为 0
        assert!(parse_points(r#"[{"instanceId":"i-1"}]"#).is_empty());
        // 数值键存在但非数值（如字符串 "91.2%"）→ as_f64 为 None → 跳过
        assert!(parse_points(r#"[{"instanceId":"i-1","Average":"91.2%"}]"#).is_empty());
    }
}
