# 设计：磁盘占用检查（`disk` 子命令）

日期：2026-08-10
状态：已批准（头脑风暴完成）

## 背景与目标

在 ops-console 中新增磁盘占用检查：对阿里云轻量应用服务器（SWAS）与云服务器（ECS）的磁盘使用率进行巡检，**使用率达到阈值（默认 90%）时通知**；同时把**查不到监控数据的实例单独列出并通知**（提醒安装云监控插件），避免"没装插件 = 静默漏报"。

经调研确认的能力边界：

- **SWAS**：一级 API `DescribeMonitorData`（`MetricName=DISKUSAGE_USED`，单位 bytes）直接返回磁盘已用空间；总量取 `ListDisks` 的 `Size`（GB）字段（系统盘）。
- **ECS**：自身 API（`DescribeDisks` / `DescribeDiskMonitorData`）**不返回文件系统空间占用**，必须走云监控（CMS）：命名空间 `acs_ecs_dashboard`、指标 `diskusage_utilization`（按 `device` 维度，如 `/dev/vda1`），查询接口 `DescribeMetricLast`。该指标依赖实例内运行云监控插件，无插件则无数据。

## 已确认的决策

| # | 决策点 | 选择 |
|---|---|---|
| 1 | 通知策略 | 超阈值每次运行都通知（无持久化，与 expiry 一致）；数据缺失同样通知 |
| 2 | 数据缺失处理 | 单独列出并通知（"疑似未装云监控插件"），与超阈值分开标记 |
| 3 | 阈值 | 命令行 `--threshold` 可配，默认 90（浮点百分比） |
| 4 | 检查范围 | 只检查 `Running` 状态实例，停止的直接跳过（控制台标注跳过数） |
| 5 | 命令形态 | 独立 `disk` 子命令，cron 独立调度 |

## 命令形态

```
ops-console disk [--threshold 90]
```

- `--threshold`：浮点百分比，默认 90；校验 `0 < threshold <= 100`
- 沿用全局 `--project` / `--provider` 过滤
- 推荐 cron 独立一行（如每 6 小时跑一次），与 expiry 互不干扰

## 数据流

```
disk 分支（main.rs，镜像 expiry 结构）
├─ select_projects → 遍历项目
│  └─ 遍历服务商 kind（aliyun）
│     ├─ SWAS 检查: ListInstances → 过滤 Running
│     │   └─ 每台: DescribeMonitorData(DISKUSAGE_USED, Period=300, 近 10 分钟窗口)
│     │           + ListDisks(Size) → 使用率 = used / (系统盘 Size GB × 1024³)
│     ├─ ECS 检查: DescribeInstances → 过滤 Running
│     │   └─ 一次 DescribeMetricLast(acs_ecs_dashboard, diskusage_utilization,
│     │       不带 Dimensions → 返回地域内全部实例数据)
│     │       → 按 instanceId 聚合各 device 取最大使用率 → 与实例列表 join 取名
│     └─ 汇总: 超阈值列表 + 数据缺失列表
├─ render → 一条通知；两表皆空则不通知
```

关键点：

- ECS 的 `DescribeMetricLast` **不带 Dimensions** 一次拿全地域实例，避免每实例一次 API 调用。
- 无数据 ≠ 错误：Running 实例在结果中找不到对应数据 → 归入"数据缺失"分支。
- 监控数据有分钟级延迟，告警判定以查询到的最新值为准（每日/6h 巡检频率下可接受）。

## 组件改动

| 文件 | 改动 |
|---|---|
| `src/cloud/aliyun/swas.rs` | `SwasDisk` 补 `Size`（i32，GB）；新增 `disk_usage_used(&self, instance_id) -> Result<Option<u64>>`（`DescribeMonitorData`，取最新 Datapoints 值；无数据返回 `Ok(None)`） |
| `src/cloud/aliyun/cms.rs`（新） | `CmsClient`（product=`metrics`，version=`2019-01-01`）；`describe_metric_last(namespace, metric, dims: Option<&str>) -> Result<Vec<MetricPoint>>`；`MetricPoint { instance_id, device, average }`，字段灵活解析（serde_json::Value 兼容键名差异） |
| `src/cloud/aliyun/mod.rs` | `pub mod cms;`；`AliyunProvider` 增加 `cms` 字段与 `pub fn cms(&self) -> &cms::CmsClient` |
| `src/ops/disk.rs`（新） | `DiskStatus` 模型、`check_swas_disk(client: &SwasClient, threshold: f64)`、`check_ecs_disk(client: &EcsClient, cms: &CmsClient, threshold: f64)`、`render_disk(items, missing)` + 单元测试 |
| `src/ops/mod.rs` | `pub mod disk;` |
| `src/main.rs` | `Command::Disk { threshold }` 子命令 + `run_provider_disk_swas` / `run_provider_disk_ecs` 分发（复用 `provider_kinds`，ECS 用 `aliyun-ecs` 标记区分） |

不需要改动：`sign.rs`（已对全部参数 percent-encode，JSON 字符串 Dimensions 可直接传入）、`rpc.rs`（分页/错误处理通用）。

## 数据模型与判定

```rust
pub struct DiskStatus {
    pub server: Server,          // 复用 cloud::Server（含 id/name/region/status）
    pub utilization: Option<f64>, // 使用率（%）；None = 数据缺失
    pub used_bytes: u64,          // 展示用
    pub total_bytes: u64,         // 展示用
    pub detail: String,           // SWAS: "系统盘"；ECS: 设备明细
}
```

- **超阈值**：`utilization >= threshold`（达到 90% 即告警）
- **数据缺失**：`utilization.is_none()` → 归缺失表
- **ECS 聚合**：一台实例多个 device → `utilization` 取最大值，`detail` 列全部设备（`/dev/vda1 95.0%, /dev/vdb1 30.1%`）；取 `Average` 统计值（磁盘使用率缓变，平均更稳）
- **SWAS 总量**：`ListDisks` 中 `DiskType == "System"` 的 `Size`（GB）× 1024³；找不到 System 盘则用第一块盘；都失败 → 数据缺失

## 渲染与通知

```
=== 磁盘占用检查 ===
- demo/aliyun: web (lh-xxx) 91.2% (已用 45.6G/50G) 系统盘
- demo/aliyun-ecs: db (i-xxx) 95.0% (设备: /dev/vda1 95.0%, /dev/vdb1 30.1%)

=== 数据缺失 ===
- demo/aliyun-ecs: cache (i-xxx) 无磁盘监控数据（疑似未装云监控插件）
```

- 标题：`磁盘占用检查: N 台超阈值, M 台数据缺失`；两表皆空则只输出 `全部实例磁盘正常`，不通知
- 通知发送失败仅 warn，不影响退出码（沿用现有模式）
- 服务商级失败收集，最后汇总后非零退出（沿用 expiry 模式）

## 错误处理

- 单实例检查失败不阻断其余实例（SWAS 逐台 try，收集错误）
- 无数据 ≠ 错误：归入缺失分支
- `DescribeMetricLast` 整体调用失败 → 该服务商记入错误列表
- 阈值参数非法 → 启动即报错（`0 < threshold <= 100`）

## 测试

- **纯逻辑**（ops/disk.rs）：
  - `>=` 边界：89.9 不告警 / 90.0 告警
  - 缺失判定：`utilization = None` → 缺失表
  - ECS 多设备聚合取最大；detail 渲染
  - render 输出包含/不包含各分支
- **反序列化**（swas.rs / cms.rs）：样例 JSON（DescribeMonitorData、DescribeMetricLast 响应结构）

## RAM 权限（文档补充）

- SWAS：`swas-open:DescribeMonitorData`（`ListDisks` 已有）
- CMS：`cms:DescribeMetricLast`（RAM 控制台展示名为 `cms:QueryMetricLast` 别名）
- 用户已为 RAM 角色添加对应权限

## 风险与备注

- CMS 地域端点 `metrics.{region}.aliyuncs.com` 已验证存在（cn-shenzhen / cn-hangzhou / cn-beijing 返回 400 即服务可达）。
- ECS 磁盘使用率依赖云监控插件；插件未装/异常时进入"数据缺失"分支提醒，不静默。
- 监控数据分钟级延迟：告警以最新上报值为准，不追历史。
