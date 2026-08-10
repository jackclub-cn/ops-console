# 磁盘占用检查（disk 子命令）实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 新增 `disk` 子命令：巡检 SWAS（`DescribeMonitorData`）与 ECS（云监控 `diskusage_utilization`）磁盘使用率，达到阈值（默认 90%，`--threshold` 可配）或数据缺失时输出并汇总一条钉钉通知。

**架构：** SWAS 侧复用现有 RpcClient（product=`swas`，一级 API）；ECS 侧新增云监控客户端（product=`metrics`，响应成功码是 `Code="200"` 而非 `"Success"`，须给 RpcClient 加自定义成功码）；纯逻辑（判定/聚合/渲染）集中在 `ops/disk.rs` 可单测，网络面 check 函数保持薄层。沿用 `ops/expiry.rs` / `ops/ecs.rs` 的「拉数据 → 纯逻辑过滤 → render → 汇总一条通知」模式。

**技术栈：** Rust 2021 / tokio / reqwest / serde / serde_json / clap / chrono / anyhow（与现有 ops-console 一致，**无新增依赖**）。

**关键 API 事实（已调研确认）：**
- SWAS `DescribeMonitorData`（SWAS-OPEN 2020-06-01）：必填 `InstanceId`/`MetricName`/`Period`/`StartTime`/`EndTime`；`MetricName=DISKUSAGE_USED` 返回磁盘已用 bytes。响应 `Datapoints` 是 **JSON 编码的字符串数组**（如 `"[]"`），元素含 `timestamp` 与数值字段。
- SWAS `ListDisks`：磁盘字段为 `Size`（GB）。当前 `SwasDisk` 结构体没有该字段，需补。
- CMS `DescribeMetricLast`（cms 2019-01-01，endpoint `metrics.{region}.aliyuncs.com`）：必填 `Namespace`/`MetricName`，`Dimensions` 可选（不带 = 查账号全部实例）。响应 `Datapoints` 同样是 **JSON 字符串数组**，元素字段：小写 `timestamp`/`userId`/`instanceId`、大写 `Minimum`/`Average`/`Maximum`，磁盘指标带 `device` 维度。**成功响应 `Code` 字段值为 `"200"`**，且可能带无意义的 `Message`。
- 地域端点 `metrics.cn-shenzhen|hangzhou|beijing.aliyuncs.com` 已验证可达（HTTP 400 = 服务存在）。
- `sign.rs` 已对全部参数 percent-encode，JSON 字符串 `Dimensions` 可直接传。

---

## 文件结构

| 文件 | 职责 | 动作 |
|---|---|---|
| `src/cloud/aliyun/rpc.rs` | 增加 `call_ok`（自定义业务成功码）+ 抽出可测的 `business_error` | 修改 |
| `src/cloud/aliyun/swas.rs` | `SwasDisk` 补 `Size`；新增 `disk_usage_used` + 可测解析函数 | 修改 |
| `src/cloud/aliyun/cms.rs` | **新**：`CmsClient`（product=`metrics`）+ `MetricPoint` + `describe_metric_last` + 可测解析函数 | 创建 |
| `src/cloud/aliyun/mod.rs` | `pub mod cms;`；`AliyunProvider` 加 `cms` 字段 + `cms()`/`swas()` 访问器 | 修改 |
| `src/ops/disk.rs` | **新**：`DiskStatus` 模型、聚合/判定/渲染纯逻辑 + 单测 | 创建 |
| `src/ops/mod.rs` | `pub mod disk;` | 修改 |
| `src/main.rs` | `Disk` 子命令 + 分发 + 阈值校验 + 通知；顶部 doc 注释补用法 | 修改 |
| `README.md` | 功能/命令参考/cron/RAM 权限补充 | 修改 |

任务依赖顺序：1 → 2 → 3 → 4 → 5 → 6（后置任务引用的类型均由前置任务定义）。

---

### 任务 1：RpcClient 支持自定义业务成功码

**文件：**
- 修改：`src/cloud/aliyun/rpc.rs:45-98`（`call` 函数）

**背景：** 云监控（CMS）成功响应返回 `"Code": "200"`，现有 `call` 的业务错误判断是 `code != "Success"` 即报错，直接复用会把 CMS 的成功响应当错误。抽出纯函数 `business_error` 并新增 `call_ok`。

- [ ] **步骤 1：编写失败的测试**

在 `src/cloud/aliyun/rpc.rs` 的 `#[cfg(test)] mod tests`（文件末尾已有，测试 `sign_params`）中追加：

```rust
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
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p ops-console --lib cloud::aliyun::rpc::tests::test_business_error`
预期：编译失败，报 `cannot find function business_error`（红）。

- [ ] **步骤 3：实现**

将 `call` 的「业务错误检查」段（现第 84-90 行 `if let Some(code) = ...` 块）替换为对 `business_error` 的调用，并新增 `call_ok` 与纯函数。改动后 `call` 如下：

```rust
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
        let url = format!("https://{}.{}.aliyuncs.com/?{}", self.product, self.region, query);

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            return Err(anyhow!(
                "阿里云 {} API HTTP {} ({}): {}",
                self.product,
                status.as_u16(),
                action,
                truncate(&text, 500)
            ));
        }

        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("响应解析失败 ({action}): {e} => {}", truncate(&text, 300)))?;

        if let Some(err) = business_error(&value, ok_codes) {
            return Err(anyhow!("阿里云 {} 业务错误 {err}", self.product));
        }

        serde_json::from_value(value).map_err(|e| anyhow!("响应反序列化失败 ({action}): {e}"))
    }
```

在 `truncate` 函数附近新增纯函数：

```rust
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
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test -p ops-console --lib cloud::aliyun::rpc`
预期：PASS（`test_business_error` + 原有 `sign` 测试全绿）。

- [ ] **步骤 5：Commit**

```bash
git add src/cloud/aliyun/rpc.rs
git commit -m "feat: RpcClient 支持自定义业务成功码（云监控 Code=200）"
```

---

### 任务 2：SWAS 客户端 —— `Size` 字段 + `disk_usage_used`

**文件：**
- 修改：`src/cloud/aliyun/swas.rs:52-60`（`SwasDisk`）、`src/cloud/aliyun/swas.rs:94-166`（`impl SwasClient` 末尾）

- [ ] **步骤 1：编写失败的测试**

在 `src/cloud/aliyun/swas.rs` 文件末尾追加测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_size_field() {
        let j = r#"{"DiskId":"d-xxx","DiskType":"System","DiskName":"系统盘","Size":50}"#;
        let d: SwasDisk = serde_json::from_str(j).unwrap();
        assert_eq!(d.size, 50);
    }

    #[test]
    fn test_parse_latest_usage() {
        // 取时间戳最新的点
        let s = r#"[{"timestamp": 1699219200, "Value": 1000}, {"timestamp": 1699219500, "Value": 2000}]"#;
        assert_eq!(parse_latest_usage(s), Some(2000));

        // 小写 value 兼容；缺 timestamp 按 0 处理
        let s = r#"[{"timestamp": 1699219500, "value": 3000}]"#;
        assert_eq!(parse_latest_usage(s), Some(3000));

        // Average 兜底（字段名变体）
        let s = r#"[{"timestamp": 1, "Average": 4096.0}]"#;
        assert_eq!(parse_latest_usage(s), Some(4096));

        // 空数组 / 空串 / 非 JSON / 无数值字段 → None
        assert_eq!(parse_latest_usage("[]"), None);
        assert_eq!(parse_latest_usage(""), None);
        assert_eq!(parse_latest_usage("not json"), None);
        let s = r#"[{"timestamp": 1}]"#;
        assert_eq!(parse_latest_usage(s), None);
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p ops-console --lib cloud::aliyun::swas`
预期：编译失败（`SwasDisk` 无 `size` 字段、`parse_latest_usage` 未定义）（红）。

- [ ] **步骤 3：实现**

`SwasDisk` 增加字段（保留原字段不动，在 `disk_name` 后追加）：

```rust
    #[serde(rename = "DiskName", default)]
    pub disk_name: String,
    /// 磁盘容量（GB）
    #[serde(rename = "Size", default)]
    pub size: i32,
```

在「请求/响应模型」区域（`CreateSnapshotResponse` 之后）新增响应结构：

```rust
#[derive(Debug, Deserialize)]
pub struct DescribeMonitorDataResponse {
    /// Datapoints 是 JSON 编码的字符串数组（元素含 timestamp 与数值字段）
    #[serde(rename = "Datapoints", default)]
    pub datapoints: String,
}
```

在 `impl SwasClient` 内（`delete_snapshot` 之后）新增方法：

```rust
    /// 查询实例磁盘已用空间（bytes）。
    /// 近 10 分钟窗口内无监控数据（如未装云监控插件）返回 None。
    pub async fn disk_usage_used(&self, instance_id: &str) -> Result<Option<u64>> {
        let now = chrono::Utc::now();
        let start = now - chrono::Duration::minutes(10);
        let start_s = start.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let end_s = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let resp: DescribeMonitorDataResponse = self
            .rpc
            .call(
                "DescribeMonitorData",
                SWAS_API_VERSION,
                &[
                    ("InstanceId", instance_id),
                    ("MetricName", "DISKUSAGE_USED"),
                    ("Period", "300"),
                    ("StartTime", start_s.as_str()),
                    ("EndTime", end_s.as_str()),
                ],
            )
            .await?;
        Ok(parse_latest_usage(&resp.datapoints))
    }
```

在 `impl SwasClient` 之后新增纯解析函数：

```rust
/// 解析 DescribeMonitorData 的 Datapoints（JSON 数组字符串），返回时间戳最新点的数值（bytes）。
/// 元素数值字段兼容 `Value`/`value`/`Average`；时间戳兼容 `timestamp`/`Timestamp`（缺省按 0）。
/// 空/非法/无数值字段 → None。
fn parse_latest_usage(datapoints: &str) -> Option<u64> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(datapoints).ok()?;
    let mut best: Option<(i64, u64)> = None;
    for p in arr {
        let ts = p
            .get("timestamp")
            .or_else(|| p.get("Timestamp"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let val = p
            .get("Value")
            .or_else(|| p.get("value"))
            .or_else(|| p.get("Average"))
            .and_then(|v| v.as_f64())
            .map(|f| f as u64);
        if let Some(v) = val {
            if best.map_or(true, |(bt, _)| ts > bt) {
                best = Some((ts, v));
            }
        }
    }
    best.map(|(_, v)| v)
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test -p ops-console --lib cloud::aliyun::swas`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add src/cloud/aliyun/swas.rs
git commit -m "feat: SWAS 磁盘已用空间查询（DescribeMonitorData）+ ListDisks Size 字段"
```

---

### 任务 3：云监控客户端（新文件 `cms.rs`）+ provider 接线

**文件：**
- 创建：`src/cloud/aliyun/cms.rs`
- 修改：`src/cloud/aliyun/mod.rs:3-6`（`pub mod`）、`src/cloud/aliyun/mod.rs:14-42`（`AliyunProvider`）

- [ ] **步骤 1：创建文件并编写失败的测试**

创建 `src/cloud/aliyun/cms.rs`，先只写测试模块与必要的 `use`（`parse_points` 未定义 → 编译红）：

```rust
//! 阿里云云监控（CMS）API 封装 —— ECS 磁盘使用率等操作系统监控指标。
//!
//! endpoint: https://metrics.{region}.aliyuncs.com
//! API Version: 2019-01-01
//! 注意：CMS 业务成功时返回 `Code: "200"`（而非 "Success"），须用 `call_ok(..., &["200"])`。

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
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p ops-console --lib cloud::aliyun::cms`
预期：编译失败（模块无内容/`parse_points` 未定义）（红）。

- [ ] **步骤 3：实现完整模块**

用以下内容替换 `cms.rs`（测试模块保留并追加到文件末尾）：

```rust
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
/// 兼容大小写变体；缺 instanceId 的点跳过；非法输入返回空列表。
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
        let average = p
            .get("Average")
            .or_else(|| p.get("average"))
            .or_else(|| p.get("Maximum"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
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

    // ...（步骤 1 的测试模块原样保留在此）
}
```

- [ ] **步骤 4：修改 `mod.rs` 接线**

`src/cloud/aliyun/mod.rs` 三处修改：

1. 模块声明区（`pub mod ecs;` 附近）加一行：

```rust
pub mod cms;
```

2. `AliyunProvider` 结构体加字段（`ecs` 字段后）：

```rust
pub struct AliyunProvider {
    region: String,
    swas: swas::SwasClient,
    ecs: ecs::EcsClient,
    cms: cms::CmsClient,
}
```

3. `new` 构造加 `cms` 初始化，并补充/调整访问器：

```rust
    pub fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Self {
        Self {
            region: region.to_string(),
            swas: swas::SwasClient::new(access_key_id, access_key_secret, region),
            ecs: ecs::EcsClient::new(access_key_id, access_key_secret, region),
            cms: cms::CmsClient::new(access_key_id, access_key_secret, region),
        }
    }

    /// SWAS 客户端（磁盘占用检查用）
    pub fn swas(&self) -> &swas::SwasClient {
        &self.swas
    }

    /// ECS 客户端（自动快照策略检查 / 到期提醒用）
    pub fn ecs(&self) -> &ecs::EcsClient {
        &self.ecs
    }

    /// 云监控客户端（ECS 磁盘使用率等操作系统监控指标）
    pub fn cms(&self) -> &cms::CmsClient {
        &self.cms
    }
```

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test -p ops-console --lib cloud::aliyun`
预期：PASS（cms 解析 + rpc + swas 全部）。

- [ ] **步骤 6：Commit**

```bash
git add src/cloud/aliyun/cms.rs src/cloud/aliyun/mod.rs
git commit -m "feat: 云监控 CMS 客户端（DescribeMetricLast）+ AliyunProvider 接线"
```

---

### 任务 4：`ops/disk.rs` 纯逻辑（模型 + 判定 + 聚合 + 渲染）

**文件：**
- 创建：`src/ops/disk.rs`
- 修改：`src/ops/mod.rs`（加 `pub mod disk;`）

- [ ] **步骤 1：创建文件并编写失败的测试**

创建 `src/ops/disk.rs`，先写测试模块与 `use`（类型未定义 → 编译红）：

```rust
//! 磁盘占用检查纯逻辑（模型 / 判定 / 聚合 / 渲染）。
//! 网络面 check 函数见本文件下半部分（任务 5）。

use crate::cloud::Server;
use std::collections::HashMap;

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_server(name: &str) -> Server {
        Server {
            id: format!("i-{name}"),
            name: name.to_string(),
            region: "cn-shenzhen".to_string(),
            status: "Running".to_string(),
            expired_at: None,
        }
    }

    fn mk_status(name: &str, utilization: Option<f64>, used: u64, total: u64, detail: &str) -> DiskStatus {
        DiskStatus {
            server: mk_server(name),
            utilization,
            used_bytes: used,
            total_bytes: total,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn test_over_boundary() {
        // 达到 90.0 即告警（>=）
        assert!(mk_status("a", Some(90.0), 0, 0, "").over(90.0));
        assert!(mk_status("a", Some(91.2), 0, 0, "").over(90.0));
        // 89.9 不告警
        assert!(!mk_status("a", Some(89.9), 0, 0, "").over(90.0));
        // 数据缺失不归入超阈值，单独走 missing()
        assert!(!mk_status("a", None, 0, 0, "").over(90.0));
        assert!(mk_status("a", None, 0, 0, "").missing());
    }

    #[test]
    fn test_aggregate_by_instance() {
        use crate::cloud::aliyun::cms::MetricPoint;
        let pts = vec![
            MetricPoint { instance_id: "i-1".into(), device: "/dev/vda1".into(), average: 91.2 },
            MetricPoint { instance_id: "i-1".into(), device: "/dev/vdb1".into(), average: 30.1 },
            MetricPoint { instance_id: "i-2".into(), device: "/dev/vda1".into(), average: 88.0 },
        ];
        let agg = aggregate_by_instance(pts);
        assert_eq!(agg.len(), 2);
        let a = &agg["i-1"];
        assert_eq!(a.max, 91.2);
        assert_eq!(a.devices.len(), 2);
        // 设备按使用率降序
        assert_eq!(a.devices[0].0, "/dev/vda1");
        assert_eq!(agg["i-2"].max, 88.0);
    }

    #[test]
    fn test_format_devices() {
        let d = vec![("/dev/vda1".to_string(), 91.2), ("/dev/vdb1".to_string(), 30.1)];
        assert_eq!(format_devices(&d), "设备: /dev/vda1 91.2%, /dev/vdb1 30.1%");
        // 设备名为空（非常规指标）→ 退化为 "系统盘 x.x%"
        let d = vec![("".to_string(), 95.0)];
        assert_eq!(format_devices(&d), "系统盘 95.0%");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(50 * 1024 * 1024 * 1024), "50.0 GB");
    }

    #[test]
    fn test_render_disk() {
        let over = vec![(
            "demo".to_string(),
            "aliyun".to_string(),
            mk_status("web", Some(91.2), 45 * 1024 * 1024 * 1024, 50 * 1024 * 1024 * 1024, "系统盘"),
        )];
        let missing = vec![(
            "demo".to_string(),
            "aliyun-ecs".to_string(),
            mk_status("cache", None, 0, 0, ""),
        )];
        let text = render_disk(&over, &missing);
        assert!(text.contains("web (i-web) 91.2% (已用 45.0G/50.0G) 系统盘"));
        assert!(text.contains("=== 数据缺失 ==="));
        assert!(text.contains("cache (i-cache) 无磁盘监控数据（疑似未装云监控插件）"));

        // 全空 → 空字符串
        assert_eq!(render_disk(&[], &[]), "");
    }

    #[test]
    fn test_title() {
        let over = vec![("demo".into(), "aliyun".into(), mk_status("a", Some(91.0), 0, 0, ""))];
        let missing = vec![("demo".into(), "aliyun-ecs".into(), mk_status("b", None, 0, 0, ""))];
        assert_eq!(title(&over, &missing), "磁盘占用检查: 1 台超阈值, 1 台数据缺失");
        assert_eq!(title(&over, &[]), "磁盘占用检查: 1 台超阈值");
        assert_eq!(title(&[], &missing), "磁盘占用检查: 1 台数据缺失");
        assert_eq!(title(&[], &[]), "磁盘占用检查: 全部正常");
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p ops-console --lib ops::disk`
预期：编译失败（类型/函数未定义）（红）。

- [ ] **步骤 3：实现**

将 `src/ops/disk.rs` 替换为完整实现（测试模块保留在文件末尾）：

```rust
//! 磁盘占用检查：SWAS 用 DescribeMonitorData + ListDisks，ECS 用云监控 diskusage_utilization。
//!
//! 超阈值（默认 90%）与数据缺失（Running 但查不到监控数据，疑似未装云监控插件）
//! 分别汇总，随 cron 每次运行都通知（无持久化，与 expiry 一致）。

use crate::cloud::aliyun::cms::MetricPoint;
use crate::cloud::Server;
use std::collections::HashMap;

/// 单个实例的磁盘占用状态
#[derive(Debug, Clone)]
pub struct DiskStatus {
    pub server: Server,
    /// 使用率（%）；None = 数据缺失
    pub utilization: Option<f64>,
    /// 已用字节（展示用）
    pub used_bytes: u64,
    /// 总容量字节（展示用）
    pub total_bytes: u64,
    /// 数据来源描述（SWAS: "系统盘"；ECS: 设备明细）
    pub detail: String,
}

impl DiskStatus {
    /// 数据缺失：Running 但查不到监控数据
    pub fn missing(&self) -> bool {
        self.utilization.is_none()
    }

    /// 是否达到/超过阈值（>=）
    pub fn over(&self, threshold: f64) -> bool {
        matches!(self.utilization, Some(u) if u >= threshold)
    }
}

/// ECS 侧：单实例的聚合结果（各设备中最大使用率 + 设备明细）
#[derive(Debug, Clone)]
pub struct InstanceUsage {
    /// 设备中最大的 Average 使用率（%）
    pub max: f64,
    /// (设备名, 使用率) 明细，按使用率降序
    pub devices: Vec<(String, f64)>,
}

/// 按 instanceId 聚合监控数据点：每实例取各设备最大 Average 作为使用率。
pub fn aggregate_by_instance(points: Vec<MetricPoint>) -> HashMap<String, InstanceUsage> {
    let mut map: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for p in points {
        map.entry(p.instance_id.clone()).or_default().push((p.device, p.average));
    }
    map.into_iter()
        .map(|(id, mut devices)| {
            devices.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let max = devices.first().map(|(_, v)| *v).unwrap_or(0.0);
            (id, InstanceUsage { max, devices })
        })
        .collect()
}

/// 设备明细："设备: /dev/vda1 91.2%, /dev/vdb1 30.1%"；设备名全空 → "系统盘 x.x%"
fn format_devices(devices: &[(String, f64)]) -> String {
    let parts: Vec<String> = devices
        .iter()
        .filter(|(d, _)| !d.is_empty())
        .map(|(d, v)| format!("{d} {v:.1}%"))
        .collect();
    if parts.is_empty() {
        let v = devices.first().map(|(_, v)| *v).unwrap_or(0.0);
        format!("系统盘 {v:.1}%")
    } else {
        format!("设备: {}", parts.join(", "))
    }
}

/// 字节 → 可读字符串（B/KB/MB/GB/TB，>=1024 进位，1 位小数）
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 渲染检查结果：超阈值表 + 数据缺失表（各表为空则省略对应部分）。
pub fn render_disk(
    over: &[(String, String, DiskStatus)],
    missing: &[(String, String, DiskStatus)],
) -> String {
    let mut out = String::new();
    if !over.is_empty() {
        out.push_str("=== 磁盘占用检查 ===\n");
        for (project, kind, s) in over {
            let pct = s.utilization.unwrap_or(0.0);
            out.push_str(&format!(
                "- {project}/{kind}: {} ({}) {:.1}% (已用 {}/{} {})\n",
                s.server.name,
                s.server.id,
                pct,
                format_bytes(s.used_bytes),
                format_bytes(s.total_bytes),
                s.detail
            ));
        }
    }
    if !missing.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("=== 数据缺失 ===\n");
        for (project, kind, s) in missing {
            out.push_str(&format!(
                "- {project}/{kind}: {} ({}) 无磁盘监控数据（疑似未装云监控插件）\n",
                s.server.name, s.server.id
            ));
        }
    }
    out
}

/// 通知标题：`磁盘占用检查: N 台超阈值, M 台数据缺失`（空的部分省略）
pub fn title(over: &[(String, String, DiskStatus)], missing: &[(String, String, DiskStatus)]) -> String {
    let mut parts = Vec::new();
    if !over.is_empty() {
        parts.push(format!("{} 台超阈值", over.len()));
    }
    if !missing.is_empty() {
        parts.push(format!("{} 台数据缺失", missing.len()));
    }
    if parts.is_empty() {
        "磁盘占用检查: 全部正常".to_string()
    } else {
        format!("磁盘占用检查: {}", parts.join(", "))
    }
}
```

（测试模块原样保留在文件末尾。）

- [ ] **步骤 4：`ops/mod.rs` 注册模块**

`src/ops/mod.rs` 的 `pub mod` 列表加一行：

```rust
pub mod disk;
```

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test -p ops-console --lib ops::disk`
预期：PASS（5 个测试）。

- [ ] **步骤 6：Commit**

```bash
git add src/ops/disk.rs src/ops/mod.rs
git commit -m "feat: 磁盘占用检查纯逻辑（判定/聚合/渲染）+ 单测"
```

---

### 任务 5：`ops/disk.rs` check 函数（SWAS + ECS 网络面）

**文件：**
- 修改：`src/ops/disk.rs`（在纯逻辑之后、测试模块之前追加）

**说明：** 本任务为薄层集成函数（无单测，与 `ops/ecs.rs::check_auto_snapshot` 同风格——纯逻辑已在任务 4 覆盖）。验证 = `cargo test` 全绿 + `cargo build` 通过。

- [ ] **步骤 1：实现 `check_swas_disk`**

在 `src/ops/disk.rs` 中 `title` 函数之后追加：

```rust
// ---------- 网络面 check 函数 ----------

use crate::cloud::aliyun::cms::CmsClient;
use crate::cloud::aliyun::ecs::EcsClient;
use crate::cloud::aliyun::swas::SwasClient;
use anyhow::Result;

/// 构建展示用 Server（region 留空：render 不使用）
fn server_from(name: String, id: String, status: &str) -> Server {
    Server {
        id,
        name,
        region: String::new(),
        status: status.to_string(),
        expired_at: None,
    }
}

/// 查询单台轻量实例的磁盘用量：返回 (已用 bytes, 总 bytes, 来源描述)；无数据 → None。
async fn disk_usage_swas(client: &SwasClient, instance_id: &str) -> Result<Option<(u64, u64, String)>> {
    let used = match client.disk_usage_used(instance_id).await? {
        Some(u) => u,
        None => return Ok(None),
    };
    let disks = client.list_disks(instance_id).await?;
    // 优先系统盘，退而求其次第一块盘
    let disk = disks
        .iter()
        .find(|d| d.disk_type == "System")
        .or_else(|| disks.first());
    let Some(disk) = disk else { return Ok(None) };
    if disk.size <= 0 {
        return Ok(None);
    }
    let total = disk.size as u64 * 1024 * 1024 * 1024;
    Ok(Some((used, total, "系统盘".to_string())))
}

/// SWAS 磁盘检查：只查 Running 实例；返回 (超阈值列表, 数据缺失列表)。
/// 单实例查询失败 → println 告警并跳过（不归入缺失，避免把 API 错误误报成插件问题）。
pub async fn check_swas_disk(
    client: &SwasClient,
    threshold: f64,
) -> Result<(Vec<DiskStatus>, Vec<DiskStatus>)> {
    let instances = client.list_instances().await?;
    let total = instances.len();
    let running: Vec<_> = instances.into_iter().filter(|i| i.status == "Running").collect();
    println!("  轻量实例 {total} 台，跳过 {} 台非 Running", total - running.len());

    let mut over = Vec::new();
    let mut missing = Vec::new();
    for inst in running {
        let id = inst.instance_id.clone();
        let name = if inst.instance_name.is_empty() {
            id.clone()
        } else {
            inst.instance_name.clone()
        };
        match disk_usage_swas(client, &id).await {
            Ok(Some((used, total_bytes, detail))) => {
                let utilization = used as f64 / total_bytes as f64 * 100.0;
                let status = DiskStatus {
                    server: server_from(name.clone(), id.clone(), &inst.status),
                    utilization: Some(utilization),
                    used_bytes: used,
                    total_bytes,
                    detail,
                };
                if status.over(threshold) {
                    over.push(status);
                }
            }
            Ok(None) => missing.push(DiskStatus {
                server: server_from(name.clone(), id.clone(), &inst.status),
                utilization: None,
                used_bytes: 0,
                total_bytes: 0,
                detail: String::new(),
            }),
            Err(e) => println!("    实例 {name} ({id}) 磁盘数据查询失败: {e:#}"),
        }
    }
    Ok((over, missing))
}
```

- [ ] **步骤 2：实现 `check_ecs_disk`**

继续追加：

```rust
/// ECS 磁盘检查：一次 DescribeMetricLast 拿地域内全部实例的 diskusage_utilization，
/// 按 instanceId 聚合设备取最大；只查 Running 实例；返回 (超阈值, 数据缺失)。
pub async fn check_ecs_disk(
    client: &EcsClient,
    cms: &CmsClient,
    threshold: f64,
) -> Result<(Vec<DiskStatus>, Vec<DiskStatus>)> {
    let servers = client.list_servers().await?;
    let total = servers.len();
    let running: Vec<Server> = servers.into_iter().filter(|s| s.status == "Running").collect();
    println!("  ECS 实例 {total} 台，跳过 {} 台非 Running", total - running.len());

    let points = cms
        .describe_metric_last("acs_ecs_dashboard", "diskusage_utilization", None)
        .await?;
    let agg = aggregate_by_instance(points);

    let mut over = Vec::new();
    let mut missing = Vec::new();
    for server in running {
        match agg.get(&server.id) {
            Some(u) => {
                let status = DiskStatus {
                    server,
                    utilization: Some(u.max),
                    used_bytes: 0,
                    total_bytes: 0,
                    detail: format_devices(&u.devices),
                };
                if status.over(threshold) {
                    over.push(status);
                }
            }
            None => missing.push(DiskStatus {
                server,
                utilization: None,
                used_bytes: 0,
                total_bytes: 0,
                detail: String::new(),
            }),
        }
    }
    Ok((over, missing))
}
```

注意：`use` 声明须放在文件顶部（与 `use crate::cloud::aliyun::cms::MetricPoint;` 一起），不要放在函数之间——把步骤 1 代码块里的 `use` 行移到文件顶部 `use std::collections::HashMap;` 附近。

- [ ] **步骤 3：运行测试 + 编译验证**

运行：`cargo test -p ops-console`
预期：PASS（全部现有 + 新增测试）；编译无警告。

运行：`cargo build`
预期：构建成功。

- [ ] **步骤 4：Commit**

```bash
git add src/ops/disk.rs
git commit -m "feat: 磁盘检查网络面（SWAS DescribeMonitorData / ECS 云监控聚合）"
```

---

### 任务 6：main.rs 接线 + README 更新

**文件：**
- 修改：`src/main.rs:52-75`（`Command` 枚举）、`src/main.rs:119-220`（match 分发，`Expiry` 分支之后）、`src/main.rs:272-300`（`run_provider_expiry` 附近）、`src/main.rs:1-10`（doc 注释）
- 修改：`README.md`（功能 / 快速开始 / 命令参考 / cron / RAM 权限）

- [ ] **步骤 1：`Command` 枚举加 `Disk` 变体**

在 `Command::Expiry` 块之后（`Expiry { ... },` 之后）：

```rust
    /// 磁盘占用检查：使用率超阈值（默认 90%）或数据缺失时输出并通知
    Disk {
        /// 磁盘使用率阈值（%），达到则告警
        #[arg(long, default_value_t = 90.0)]
        threshold: f64,
    },
```

- [ ] **步骤 2：match 分发加 `Disk` 分支**

在 `Command::Expiry { days } => { ... }` 分支的闭合 `}` 之后、match 的闭合之前，新增：

```rust
        Command::Disk { threshold } => {
            if !(threshold > 0.0 && threshold <= 100.0) {
                anyhow::bail!("--threshold 参数无效: {threshold}（范围 0 < 阈值 <= 100）");
            }

            let targets = select_projects(&cfg, cli.project.as_deref())?;
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut over: Vec<(String, String, ops::disk::DiskStatus)> = Vec::new();
            let mut missing: Vec<(String, String, ops::disk::DiskStatus)> = Vec::new();
            let mut errors = Vec::new();
            for project in &targets {
                println!("\n===== 项目: {} =====", project.name);
                // 轻量磁盘检查（kind=aliyun）
                match run_provider_disk_swas(&cfg, project, "aliyun", threshold).await {
                    Ok((o, m)) => {
                        over.extend(
                            o.into_iter().map(|s| (project.name.clone(), "aliyun".into(), s)),
                        );
                        missing.extend(
                            m.into_iter().map(|s| (project.name.clone(), "aliyun".into(), s)),
                        );
                    }
                    Err(e) => {
                        println!("服务商 aliyun 磁盘检查失败: {e:#}");
                        errors.push("aliyun".to_string());
                    }
                }
                // ECS 磁盘检查：同账号凭据，kind 标记 aliyun-ecs；
                // 仅当未指定 --provider 或指定 aliyun 时执行
                let do_ecs = match cli.provider.as_deref() {
                    Some(k) => k == "aliyun",
                    None => project.providers.contains_key("aliyun"),
                };
                if do_ecs {
                    match run_provider_disk_ecs(&cfg, project, "aliyun", threshold).await {
                        Ok((o, m)) => {
                            over.extend(
                                o.into_iter().map(|s| (project.name.clone(), "aliyun-ecs".into(), s)),
                            );
                            missing.extend(
                                m.into_iter().map(|s| (project.name.clone(), "aliyun-ecs".into(), s)),
                            );
                        }
                        Err(e) => {
                            println!("ECS 磁盘检查失败: {e:#}");
                            errors.push("aliyun-ecs".to_string());
                        }
                    }
                }
            }

            if !over.is_empty() || !missing.is_empty() {
                let text = ops::disk::render_disk(&over, &missing);
                println!("{text}");
                if let Some(n) = &notifier {
                    let title = ops::disk::title(&over, &missing);
                    if let Err(e) = n.send(&title, &text).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            } else {
                println!("全部实例磁盘正常");
            }

            if !errors.is_empty() {
                anyhow::bail!("以下服务商磁盘检查失败: {}", errors.join(", "));
            }
            Vec::new()
        }
```

- [ ] **步骤 3：新增分发函数**

在 `run_provider_expiry` 函数（`src/main.rs:272` 附近）之后追加：

```rust
/// 按服务商 kind 分发轻量磁盘检查（新服务商 = 在此加一个分支）。
async fn run_provider_disk_swas(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    threshold: f64,
) -> anyhow::Result<(Vec<ops::disk::DiskStatus>, Vec<ops::disk::DiskStatus>)> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            ops::disk::check_swas_disk(provider.swas(), threshold).await
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
    }
}

/// 按服务商 kind 分发 ECS 磁盘检查（新服务商 = 在此加一个分支）。
async fn run_provider_disk_ecs(
    cfg: &config::Config,
    project: &config::Project,
    kind: &str,
    threshold: f64,
) -> anyhow::Result<(Vec<ops::disk::DiskStatus>, Vec<ops::disk::DiskStatus>)> {
    match kind {
        "aliyun" => {
            let pcfg = cfg.provider(project, kind)?;
            let (ak, sk) = pcfg.aliyun_credentials()?;
            let provider = cloud::aliyun::AliyunProvider::new(&ak, &sk, &pcfg.region);
            ops::disk::check_ecs_disk(provider.ecs(), provider.cms(), threshold).await
        }
        other => anyhow::bail!("服务商 {other:?} 尚未实现（目前仅支持: aliyun）"),
    }
}
```

- [ ] **步骤 4：更新 main.rs 顶部 doc 注释**

在 `src/main.rs:1-10` 的 doc 注释中补两行用法示例：

```rust
//!   ops-console snapshot --keep 2        # 快照轮转 + ECS 自动快照策略检查
//!   ops-console expiry --days 30,15,3    # SWAS + ECS 到期提醒
//!   ops-console disk --threshold 90      # SWAS + ECS 磁盘占用检查（超阈值/数据缺失通知）
```

- [ ] **步骤 5：README 更新**

`README.md` 四处修改：

1. **功能列表**（`- ECS 自动快照策略检查：...` 之后）加：

```markdown
- 磁盘占用检查：SWAS（DescribeMonitorData）+ ECS（云监控 diskusage_utilization）使用率超阈值（默认 90%，--threshold 可配）或数据缺失时通知
```

2. **快速开始**（`./target/release/ops-console expiry --days 60,30,7` 示例之后）加：

```markdown
# 磁盘占用检查（默认阈值 90%，可 --threshold 调整）
./target/release/ops-console disk
./target/release/ops-console disk --threshold 85
```

3. **命令参考**（`expiry` 块之后）加：

```text
  disk [--threshold 90]
                             磁盘占用检查：SWAS + ECS 全部 Running 实例，
                             使用率达到阈值或数据缺失时输出并汇总发一条通知
                             （数据缺失 = Running 但查不到监控数据，疑似未装云监控插件）
```

4. **RAM 权限**：轻量策略 `Action` 数组加 `"swas-open:DescribeMonitorData"`；ECS 只读策略 `Action` 数组加 `"cms:QueryMetricLast"`（RAM 控制台别名，对应 `DescribeMetricLast`）。

5. **cron 示例**（`expiry` cron 块之后）加：

```bash
# 每 6 小时检查一次磁盘占用（默认阈值 90%）
0 */6 * * * DINGTALK_WEBHOOK_URL=... DINGTALK_SECRET=... \
  /opt/ops-console/target/release/ops-console \
  --config /opt/ops-console/config \
  disk >> /var/log/ops-console.log 2>&1
```

- [ ] **步骤 6：全量验证**

运行：
```bash
cargo test -p ops-console
cargo build
./target/debug/ops-console --help
./target/debug/ops-console disk --help
```

预期：
- `cargo test` 全绿（含新增 `rpc::business_error` / `swas` / `cms` / `ops::disk` 测试）
- `cargo build` 成功
- `--help` 子命令列表出现 `disk`
- `disk --help` 显示 `--threshold` 参数与默认值 `90`

- [ ] **步骤 7：Commit**

```bash
git add src/main.rs README.md
git commit -m "feat: disk 子命令接线 + README 文档更新"
```

---

## 自检记录

- **规格覆盖度：** 决策 1（超阈值每次通知，无持久化）→ Task 6 通知逻辑 + Task 4 `over()`；决策 2（数据缺失单独通知）→ Task 4 `missing()`/`render_disk`/`title` + Task 5 check 函数；决策 3（`--threshold` 可配默认 90）→ Task 6 `Command::Disk`；决策 4（只查 Running）→ Task 5 两处 `status == "Running"` 过滤；决策 5（独立 `disk` 子命令）→ Task 6。设计文档「数据流」「组件改动」「数据模型与判定」「渲染与通知」「错误处理」「测试」均有对应任务。
- **占位符扫描：** 无 TODO/待定；所有步骤含完整代码。
- **类型一致性：** `business_error`（Task 1）→ `call_ok`（Task 1）→ `CmsClient::describe_metric_last` 用 `call_ok(&["200"])`（Task 3）；`MetricPoint{instance_id,device,average}`（Task 3）→ `aggregate_by_instance(Vec<MetricPoint>)`（Task 4）→ `check_ecs_disk`（Task 5）；`DiskStatus{server,utilization,used_bytes,total_bytes,detail}` 全程一致；`check_swas_disk`/`check_ecs_disk` 返回 `(Vec<DiskStatus>, Vec<DiskStatus>)` 与 Task 6 分发函数签名一致；`SwasClient::disk_usage_used -> Result<Option<u64>>`（Task 2）与 `disk_usage_swas` 使用一致；`SwasDisk.size: i32`（Task 2）与 `disk_usage_swas` 使用一致。
- **已知取舍：** ECS `DescribeMetricLast` 单次最多 1000 条（Length 默认），超大规模账号需 NextToken 分页——超出本工具目标范围（YAGNI），设计文档已注明。
