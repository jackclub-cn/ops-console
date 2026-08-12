# Provider Driver 注册表 + 跨地域巡检消重 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 收敛服务商接入为「一处注册」（`ProviderDriver` trait + 静态注册表），并把 `main.rs` 中 5 处重复的跨地域巡检骨架抽成 `scan_regions` helper。

**Architecture:** 新增 `cloud::driver::ProviderDriver` trait（每个命令一个方法 + 能力声明 `commands()` + 凭据解析），`drivers()` 静态注册表派生 `supported_kinds()`。`AliyunDriver` 实现全部命令方法，内部复用新抽出的 `scan_regions`（权限跳过 / 失败汇总 / 全失败才报错）。`main.rs` 命令臂改为「遍历注册表 + 按能力分发」，`serve/api.rs` 的 `gather_resources` 也走注册表。

**Tech Stack:** Rust 2021，tokio，async-trait，anyhow，serde_json。无新增依赖。

## Global Constraints

- 版本: Rust edition 2021（当前工程）；依赖不可新增。
- 行为契约：CLI 的摘要输出、通知内容、错误消息文本、退出码语义**保持等价**（spec §3 明确的行为变更除外）。
- `cloud::CloudProvider` trait / `Server` / `Snapshot` 模型不变。
- 每任务结束时 `cargo check` 通过 + `cargo test` 全绿。
- 禁止调用真实阿里云 API（测试全程离线；`config/project.yml` 含真实凭据，绝不运行会触发网络请求的命令）。
- 文件名/模块路径：`cloud/driver.rs`、`cloud/aliyun/driver.rs`、`cloud/aliyun/mod.rs`、`cloud/mod.rs`、`ops/snapshot.rs`、`main.rs`、`config.rs`、`serve/api.rs`。

---

### Task 1: `scan_regions` + `RegionScan`（`cloud/aliyun/mod.rs`）

**Files:**
- Modify: `src/cloud/aliyun/mod.rs`（顶部 import + 新增函数/结构体，放在 `is_permission_error` 之后；测试加入文件底部 `mod tests`）
- Test: `src/cloud/aliyun/mod.rs` 内 `mod tests`

**Interfaces:**
- Produces:
  - `pub struct RegionScan<T> { pub items: Vec<T>, pub no_perm: Vec<String>, pub failed: Vec<(String, String)> }`
  - `impl<T> RegionScan<T> { pub fn all_failed_err(&self, label: &str) -> Option<anyhow::Error> }`
  - `pub async fn scan_regions<T, F, Fut>(groups: &[RegionGroup], label: &str, product: &str, f: F) -> RegionScan<T>` where `F: Fn(&RegionGroup) -> Fut`, `Fut: Future<Output = anyhow::Result<T>>`

- [ ] **Step 1: 顶部加 `use std::future::Future;`**

在 `src/cloud/aliyun/mod.rs` 顶部（`use std::time::Duration;` 附近）加入：

```rust
use std::future::Future;
```

- [ ] **Step 2: 写失败测试**

在 `src/cloud/aliyun/mod.rs` 文件底部 `mod tests { use super::*; ... }` 内追加：

```rust
    #[tokio::test]
    async fn test_scan_regions_all_ok() {
        let groups = vec![
            RegionGroup::new("ak", "sk", "cn-a"),
            RegionGroup::new("ak", "sk", "cn-b"),
        ];
        let scan = scan_regions(&groups, "检查", "SWAS", |_| async { Ok(1u8) }).await;
        assert_eq!(scan.items, vec![1, 1]);
        assert!(scan.no_perm.is_empty());
        assert!(scan.failed.is_empty());
    }

    #[tokio::test]
    async fn test_scan_regions_permission_skipped() {
        let groups = vec![
            RegionGroup::new("ak", "sk", "cn-a"),
            RegionGroup::new("ak", "sk", "cn-b"),
            RegionGroup::new("ak", "sk", "cn-c"),
        ];
        let scan = scan_regions(&groups, "检查", "SWAS", |g| async move {
            if g.region == "cn-b" {
                Err(anyhow::anyhow!(
                    "阿里云 swas API HTTP 403 (ListInstances): NoPermission: nope"
                ))
            } else {
                Ok(1u8)
            }
        })
        .await;
        assert_eq!(scan.no_perm, vec!["cn-b".to_string()]);
        assert_eq!(scan.items, vec![1, 1]);
        assert!(scan.failed.is_empty());
    }

    #[tokio::test]
    async fn test_scan_regions_real_error_recorded() {
        let groups = vec![
            RegionGroup::new("ak", "sk", "cn-a"),
            RegionGroup::new("ak", "sk", "cn-b"),
        ];
        let scan = scan_regions(&groups, "磁盘检查", "SWAS", |g| async move {
            if g.region == "cn-b" {
                Err(anyhow::anyhow!("dns error"))
            } else {
                Ok(1u8)
            }
        })
        .await;
        assert_eq!(scan.failed.len(), 1);
        assert_eq!(scan.failed[0].0, "cn-b");
        assert!(scan.failed[0].1.contains("dns"));
        assert_eq!(scan.items, vec![1]);
    }

    #[test]
    fn test_all_failed_err() {
        // 全失败 → Some，消息含 label 与首个错误
        let s = RegionScan::<i32> {
            items: vec![],
            no_perm: vec![],
            failed: vec![("cn-a".into(), "boom".into())],
        };
        let e = s.all_failed_err("检查").unwrap();
        assert!(e.to_string().contains("全部 1 个地域 检查 失败"));
        assert!(e.to_string().contains("boom"));
        // 有成功返回（哪怕空数据）→ None（成功返回过即证明 API 通）
        let s = RegionScan {
            items: vec![1],
            no_perm: vec![],
            failed: vec![("cn-a".into(), "boom".into())],
        };
        assert!(s.all_failed_err("检查").is_none());
        // 有权限跳过 → None（权限跳过不算总失败）
        let s = RegionScan::<i32> {
            items: vec![],
            no_perm: vec!["cn-a".into()],
            failed: vec![("cn-b".into(), "boom".into())],
        };
        assert!(s.all_failed_err("检查").is_none());
        // 无失败 → None
        let s = RegionScan::<i32> {
            items: vec![],
            no_perm: vec![],
            failed: vec![],
        };
        assert!(s.all_failed_err("检查").is_none());
    }
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test scan_regions -- --nocapture` 与 `cargo test all_failed_err`
Expected: 编译失败（`scan_regions` / `RegionScan` 未定义）。

- [ ] **Step 4: 实现 `RegionScan` + `scan_regions`**

在 `src/cloud/aliyun/mod.rs` 中 `pub fn is_permission_error(...)` 函数之后插入：

```rust
/// 跨地域巡检结果。
pub struct RegionScan<T> {
    /// 每个成功地域的结果（类型由闭包决定，可为元组）
    pub items: Vec<T>,
    /// 权限类错误（无权限/未开通）跳过的地域
    pub no_perm: Vec<String>,
    /// 非权限失败：`(地域, 错误信息)`
    pub failed: Vec<(String, String)>,
}

impl<T> RegionScan<T> {
    /// 全部地域失败且无任何产出与权限跳过 → 返回总失败错误；否则 None。
    ///
    /// 判定「无任何地域成功返回」而非「合并结果为空」：地域成功返回过（哪怕数据为空）
    /// 即证明 API 通，不应升级为总失败。
    pub fn all_failed_err(&self, label: &str) -> Option<anyhow::Error> {
        if !self.failed.is_empty() && self.items.is_empty() && self.no_perm.is_empty() {
            let n = self.failed.len();
            let first = &self.failed[0].1;
            Some(anyhow!("全部 {n} 个地域 {label} 失败（首个错误: {first}）"))
        } else {
            None
        }
    }
}

/// 跨地域巡检：对每个地域执行 `f`，按错误类型分类汇总，并打印跳过提示。
///
/// - 权限类错误（[`is_permission_error`]）→ 记入 `no_perm`，打印「无 {product} 权限」跳过提示
/// - 非权限错误 → 记入 `failed`，打印「{label}失败」跳过提示
/// 是否升级为总失败由调用方用 [`RegionScan::all_failed_err`] 决定（单资源族返回 Err，
/// 磁盘等双资源族按族分别记录）。
pub async fn scan_regions<T, F, Fut>(
    groups: &[RegionGroup],
    label: &str,
    product: &str,
    f: F,
) -> RegionScan<T>
where
    F: Fn(&RegionGroup) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut scan = RegionScan {
        items: Vec::new(),
        no_perm: Vec::new(),
        failed: Vec::new(),
    };
    for g in groups {
        match f(g).await {
            Ok(v) => scan.items.push(v),
            Err(e) if is_permission_error(&e) => scan.no_perm.push(g.region.clone()),
            Err(e) => scan.failed.push((g.region.clone(), format!("{e:#}"))),
        }
    }
    if !scan.no_perm.is_empty() {
        println!(
            "  跳过 {} 个地域（无 {product} 权限，可能未购买/未授权）: {}",
            scan.no_perm.len(),
            scan.no_perm.join(", ")
        );
    }
    if !scan.failed.is_empty() {
        println!(
            "  跳过 {} 个地域（{label}失败）: {}",
            scan.failed.len(),
            scan
                .failed
                .iter()
                .map(|(r, _)| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    scan
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test scan_regions -- --nocapture` 与 `cargo test all_failed_err`
Expected: 4 个新测试全 PASS。

- [ ] **Step 6: 全量回归 + 提交**

Run: `cargo check` 与 `cargo test`
Expected: 编译通过，66+4 个测试全绿。

```bash
git add src/cloud/aliyun/mod.rs
git commit -m "feat: 跨地域巡检 scan_regions helper + RegionScan 总失败判定

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: `cloud/driver.rs` 注册表骨架 + `AliyunDriver`（kind/commands/credentials）

**Files:**
- Create: `src/cloud/driver.rs`
- Create: `src/cloud/aliyun/driver.rs`
- Modify: `src/cloud/mod.rs`（声明 `pub mod driver;`）
- Modify: `src/cloud/aliyun/mod.rs`（声明 `pub mod driver;` + `pub use driver::AliyunDriver;`）

**Interfaces:**
- Produces:
  - `pub enum Command { Snapshot, Expiry, ExpiryDomain, Disk, EcsAutosnapshot, EcsExpiry }`（`Debug, Clone, Copy, PartialEq, Eq`）
  - `pub struct DiskGroup { pub label: String, pub over: Vec<DiskStatus>, pub missing: Vec<DiskStatus>, pub error: Option<String> }`
  - `pub struct DiskGroups { pub groups: Vec<DiskGroup> }`
  - `pub trait ProviderDriver: Send + Sync { fn kind(&self) -> &'static str; fn commands(&self) -> &[Command] { &[] } fn credentials(&self, pcfg: &ProviderConfig) -> anyhow::Result<(String, String)>; async fn rotate/expiry/domain_expiry/disk/ecs_autosnapshot/ecs_expiry/resources(&self, cfg: &Config, project: &Project, ...) -> anyhow::Result<...> }`（后 7 个方法默认 `bail!("服务商 {} 不支持 XXX", self.kind())`）
  - `pub fn drivers() -> &'static [&'static dyn ProviderDriver]`
  - `pub fn driver(kind: &str) -> Option<&'static dyn ProviderDriver>`
  - `pub fn supported_kinds() -> Vec<&'static str>`
  - `pub struct AliyunDriver;`（`cloud::aliyun::AliyunDriver`，实现 kind="aliyun"、commands=6、credentials）

- [ ] **Step 1: 写失败测试**

新建 `src/cloud/driver.rs`，先写入测试（内容见 Step 4，测试在文件底部 `mod tests`；此时引用 `AliyunDriver` 尚不存在，会编译失败）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test driver:: -- --nocapture`
Expected: 编译失败（`AliyunDriver` 未定义）。

- [ ] **Step 3: 实现 `cloud/driver.rs`**

写入完整文件：

```rust
//! 服务商驱动注册表：命令分发 + 凭据解析的唯一入口。
//!
//! 接入新服务商 = 实现 [`ProviderDriver`] + 在 [`drivers`] 注册一行；
//! [`supported_kinds`] 与命令分发均从注册表派生，无需再同步其它位置。

use crate::config::{Config, Project, ProviderConfig};
use crate::ops::disk::DiskStatus;
use crate::ops::ecs::AutoSnapshotStatus;
use crate::ops::expiry::{DomainAlert, ExpiryAlert};
use anyhow::{anyhow, Result};

/// 服务商可执行的命令类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 快照轮转
    Snapshot,
    /// 服务器到期提醒
    Expiry,
    /// 域名到期提醒
    ExpiryDomain,
    /// 磁盘占用检查
    Disk,
    /// ECS 自动快照策略检查（随 snapshot 执行）
    EcsAutosnapshot,
    /// ECS 到期提醒
    EcsExpiry,
}

/// 磁盘检查的一个资源族结果（label 由驱动决定，如 aliyun 的 "aliyun" / "aliyun-ecs"）。
#[derive(Debug, Default)]
pub struct DiskGroup {
    pub label: String,
    pub over: Vec<DiskStatus>,
    pub missing: Vec<DiskStatus>,
    /// 该资源族全部地域失败时 Some（错误信息）；成功时 None
    pub error: Option<String>,
}

/// 磁盘检查全部结果（可能多个资源族）。
#[derive(Debug, Default)]
pub struct DiskGroups {
    pub groups: Vec<DiskGroup>,
}

/// 服务商驱动：一个服务商的一种接入实现。
#[async_trait::async_trait]
pub trait ProviderDriver: Send + Sync {
    /// 唯一 kind（与 project.yml 中 `providers.<kind>` 键一致）。
    fn kind(&self) -> &'static str;

    /// 支持的命令列表（默认不支持任何命令；逐项覆盖）。
    /// 未声明的命令 main 层不会调用对应方法。
    fn commands(&self) -> &[Command] {
        &[]
    }

    /// 解析项目下该服务商的凭据。
    /// 默认实现读取通用字段 `access_key_id` / `access_key_secret`（环境变量已在配置加载时注入）。
    fn credentials(&self, pcfg: &ProviderConfig) -> Result<(String, String)> {
        let id = pcfg
            .access_key_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少 {} 凭据 AccessKeyId：请填写 project.yml 或设置环境变量",
                    self.kind()
                )
            })?;
        let secret = pcfg
            .access_key_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少 {} 凭据 AccessKeySecret：请填写 project.yml 或设置环境变量",
                    self.kind()
                )
            })?;
        Ok((id, secret))
    }

    /// 快照轮转。
    async fn rotate(
        &self,
        _cfg: &Config,
        _project: &Project,
        _keep: usize,
        _wait_minutes: u64,
    ) -> Result<()> {
        anyhow::bail!("服务商 {} 不支持快照轮转", self.kind())
    }

    /// 服务器到期提醒（主资源）。
    async fn expiry(
        &self,
        _cfg: &Config,
        _project: &Project,
        _thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        anyhow::bail!("服务商 {} 不支持到期提醒", self.kind())
    }

    /// 域名到期提醒（账号级全局资源）。
    async fn domain_expiry(
        &self,
        _cfg: &Config,
        _project: &Project,
        _thresholds: &[i64],
    ) -> Result<Vec<DomainAlert>> {
        anyhow::bail!("服务商 {} 不支持域名到期提醒", self.kind())
    }

    /// 磁盘占用检查（返回按资源族分组）。
    async fn disk(
        &self,
        _cfg: &Config,
        _project: &Project,
        _threshold: f64,
    ) -> Result<DiskGroups> {
        anyhow::bail!("服务商 {} 不支持磁盘检查", self.kind())
    }

    /// ECS 自动快照策略检查。
    async fn ecs_autosnapshot(
        &self,
        _cfg: &Config,
        _project: &Project,
    ) -> Result<Vec<AutoSnapshotStatus>> {
        anyhow::bail!("服务商 {} 不支持自动快照检查", self.kind())
    }

    /// ECS 到期提醒。
    async fn ecs_expiry(
        &self,
        _cfg: &Config,
        _project: &Project,
        _thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        anyhow::bail!("服务商 {} 不支持 ECS 到期", self.kind())
    }

    /// 资源快照（Web 资源列表）：返回 `{ <resource_kind>: {...} }` JSON。
    async fn resources(
        &self,
        _cfg: &Config,
        _project: &Project,
    ) -> Result<serde_json::Value> {
        anyhow::bail!("服务商 {} 不支持资源列表", self.kind())
    }
}

/// 全部已注册驱动（唯一权威来源；`supported_kinds` 由此派生）。
pub fn drivers() -> &'static [&'static dyn ProviderDriver] {
    &[crate::cloud::aliyun::AliyunDriver]
}

/// 按 kind 查驱动。
pub fn driver(kind: &str) -> Option<&'static dyn ProviderDriver> {
    drivers().iter().find(|d| d.kind() == kind).copied()
}

/// 支持的服务商 kind 列表（前端下拉、`--provider` 校验统一用）。
pub fn supported_kinds() -> Vec<&'static str> {
    drivers().iter().map(|d| d.kind()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_driver_lookup() {
        assert!(driver("aliyun").is_some());
        assert!(driver("tencent").is_none());
    }

    #[test]
    fn test_supported_kinds() {
        assert_eq!(supported_kinds(), vec!["aliyun"]);
    }

    #[test]
    fn test_drivers_unique_kinds() {
        let mut kinds: Vec<&str> = drivers().iter().map(|d| d.kind()).collect();
        kinds.sort_unstable();
        let mut dedup = kinds.clone();
        dedup.dedup();
        assert_eq!(kinds, dedup, "驱动 kind 必须唯一");
    }
}
```

- [ ] **Step 4: 实现 `cloud/aliyun/driver.rs`（骨架）**

新建文件：

```rust
//! 阿里云驱动实现：命令分发 + 凭据解析。
//!
//! 命令方法（rotate / expiry / disk ...）在后续任务补齐，当前仅实现 kind / commands / credentials。

use crate::cloud::driver::{Command, ProviderDriver};
use crate::config::ProviderConfig;
use anyhow::{anyhow, Result};

/// 阿里云服务商驱动（SWAS 轻量 + ECS + 域名）。
pub struct AliyunDriver;

#[async_trait::async_trait]
impl ProviderDriver for AliyunDriver {
    fn kind(&self) -> &'static str {
        "aliyun"
    }

    fn commands(&self) -> &[Command] {
        &[
            Command::Snapshot,
            Command::Expiry,
            Command::ExpiryDomain,
            Command::Disk,
            Command::EcsAutosnapshot,
            Command::EcsExpiry,
        ]
    }

    fn credentials(&self, pcfg: &ProviderConfig) -> Result<(String, String)> {
        // 环境变量（ALIYUN_ACCESS_KEY_ID / ALIYUN_ACCESS_KEY_SECRET）已在 Config::load 时注入，
        // 此处只读字段并报错。错误消息保留旧 aliyun_credentials 的语义。
        let id = pcfg
            .access_key_id
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeyId：请填写 project.yml 或设置环境变量 ALIYUN_ACCESS_KEY_ID"
                )
            })?;
        let secret = pcfg
            .access_key_secret
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "缺少阿里云 AccessKeySecret：请填写 project.yml 或设置环境变量 ALIYUN_ACCESS_KEY_SECRET"
                )
            })?;
        Ok((id, secret))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_and_commands() {
        let d = AliyunDriver;
        assert_eq!(d.kind(), "aliyun");
        assert!(d.commands().contains(&Command::Snapshot));
        assert!(d.commands().contains(&Command::EcsExpiry));
        assert_eq!(d.commands().len(), 6);
    }

    #[test]
    fn test_credentials_missing() {
        let pcfg = ProviderConfig::default();
        let err = AliyunDriver.credentials(&pcfg).unwrap_err();
        assert!(err.to_string().contains("AccessKeyId"));
    }

    #[test]
    fn test_credentials_ok() {
        let pcfg = ProviderConfig {
            region: "cn-shenzhen".into(),
            access_key_id: Some("AKID".into()),
            access_key_secret: Some("SECRET".into()),
        };
        let (id, secret) = AliyunDriver.credentials(&pcfg).unwrap();
        assert_eq!(id, "AKID");
        assert_eq!(secret, "SECRET");
    }
}
```

- [ ] **Step 5: 声明子模块**

在 `src/cloud/mod.rs` 顶部模块声明处加入（保留 `pub mod aliyun;`）：

```rust
pub mod driver;
```

在 `src/cloud/aliyun/mod.rs` 顶部模块声明处（`pub mod cms;` 等之后）加入：

```rust
pub mod driver;
pub use driver::AliyunDriver;
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test driver:: -- --nocapture` 与 `cargo test aliyun::driver:: -- --nocapture`
Expected: 5 个新测试全 PASS，`cargo check` 通过。

- [ ] **Step 7: 全量回归 + 提交**

Run: `cargo test`
Expected: 全绿（现有 `main.rs` / `api.rs` 尚未改动，不受影响）。

```bash
git add src/cloud/driver.rs src/cloud/aliyun/driver.rs src/cloud/mod.rs src/cloud/aliyun/mod.rs
git commit -m "feat: ProviderDriver trait + 静态注册表（drivers/driver/supported_kinds）+ AliyunDriver 骨架

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: `AliyunProvider::list_servers` 改用 `scan_regions`

**Files:**
- Modify: `src/cloud/aliyun/mod.rs`（`impl CloudProvider for AliyunProvider { async fn list_servers }`）

**Interfaces:**
- Consumes: `scan_regions` / `RegionScan::all_failed_err`（Task 1）、`RegionGroup`（现有）
- Produces: `list_servers` 行为等价（跳过提示从 `tracing::warn` 改为 `println`，spec §3 B2）

- [ ] **Step 1: 替换 `list_servers` 实现**

定位 `src/cloud/aliyun/mod.rs` 中 `impl CloudProvider for AliyunProvider` 的 `async fn list_servers(&self) -> Result<Vec<Server>>`（约 190-252 行），整体替换为：

```rust
    async fn list_servers(&self) -> Result<Vec<Server>> {
        let scan = scan_regions(&self.groups, "SWAS 查询", "SWAS", |g| async move {
            let region = g.region.clone();
            let instances = g.swas.list_instances().await?;
            Ok(instances
                .into_iter()
                .map(|i| {
                    let id = i.instance_id.clone();
                    Server {
                        id,
                        name: if i.instance_name.is_empty() {
                            i.instance_id
                        } else {
                            i.instance_name
                        },
                        region: region.clone(),
                        status: i.status,
                        expired_at: parse_expired_time(&i.expired_time),
                    }
                })
                .collect::<Vec<_>>())
        })
        .await;
        if let Some(e) = scan.all_failed_err("SWAS 查询") {
            return Err(e);
        }
        let mut out = Vec::new();
        for servers in scan.items {
            out.extend(servers);
        }
        Ok(out)
    }
```

- [ ] **Step 2: 编译 + 回归**

Run: `cargo check` 与 `cargo test`
Expected: 编译通过，测试全绿（`list_servers` 无单元测试，命中网络；改动由编译与既有测试守护）。

- [ ] **Step 3: 提交**

```bash
git add src/cloud/aliyun/mod.rs
git commit -m "refactor: list_servers 复用 scan_regions（跳过提示统一为 println）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: `rotate_provider` 移到 `ops/snapshot.rs`

**Files:**
- Modify: `src/ops/snapshot.rs`（新增 `rotate_provider` + import）
- Modify: `src/main.rs`（删除本地 `rotate_provider`，`run_provider_rotate` 改调 `ops::snapshot::rotate_provider`）

**Interfaces:**
- Produces: `pub async fn rotate_provider<P: CloudProvider + ?Sized>(provider: &P, notify_cfg: &NotifyConfig, keep: usize, wait_minutes: u64) -> anyhow::Result<()>` in `ops::snapshot`

- [ ] **Step 1: 在 `ops/snapshot.rs` 新增函数与 import**

在 `src/ops/snapshot.rs` 顶部 import 区追加：

```rust
use crate::config::NotifyConfig;
use crate::notify;
use std::time::Duration;
```

（若与现有 `use std::time::Duration;` 重复则合并。）在文件末尾追加：

```rust
/// 对单服务商的全部实例执行轮转（只依赖 CloudProvider trait，与具体服务商无关）。
pub async fn rotate_provider<P: CloudProvider + ?Sized>(
    provider: &P,
    notify_cfg: &NotifyConfig,
    keep: usize,
    wait_minutes: u64,
) -> anyhow::Result<()> {
    let servers = provider.list_servers().await?;
    if servers.is_empty() {
        println!("  无实例，跳过");
        return Ok(());
    }

    let notifier = notify::from_config(notify_cfg)?;
    let mut errors = Vec::new();
    for server in &servers {
        println!("  -- 实例: {} ({})", server.name, server.id);
        match rotate(
            provider,
            &server.id,
            keep,
            Duration::from_secs(wait_minutes * 60),
        )
        .await
        {
            Ok(summary) => {
                print!("{}", summary.render());
                // 通知渠道发送结果（失败不阻断主流程，避免告警本身导致退出码非零）
                if let Some(n) = &notifier {
                    let title = format!("快照轮转: {} 成功", summary.server_name);
                    if let Err(e) = n.send(&title, &summary.render()).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            }
            Err(e) => {
                println!("    实例 {} 轮转失败: {e:#}", server.name);
                errors.push(server.name.clone());
                // 失败也发钉钉通知（及时告警，不依赖 cron 日志回看）
                if let Some(n) = &notifier {
                    let title = format!("快照轮转失败: {}", server.name);
                    let text = format!("实例 {} ({}) 快照轮转失败:\n{e:#}", server.name, server.id);
                    if let Err(se) = n.send(&title, &text).await {
                        tracing::warn!("失败通知发送失败: {se}");
                    }
                }
            }
        }
    }
    if !errors.is_empty() {
        anyhow::bail!("以下实例轮转失败: {}", errors.join(", "));
    }
    Ok(())
}
```

- [ ] **Step 2: 删除 `main.rs` 中的旧 `rotate_provider` 并改引用**

在 `src/main.rs`：
1. 删除文件末尾的本地 `async fn rotate_provider<P: CloudProvider + ?Sized>(...)`（约 788-840 行）整块。
2. 在 `run_provider_rotate` 中把 `rotate_provider(&provider, &cfg.notify, keep, wait_minutes).await` 改为 `ops::snapshot::rotate_provider(&provider, &cfg.notify, keep, wait_minutes).await`。

- [ ] **Step 3: 编译 + 回归**

Run: `cargo check` 与 `cargo test`
Expected: 编译通过，测试全绿。

- [ ] **Step 4: 提交**

```bash
git add src/ops/snapshot.rs src/main.rs
git commit -m "refactor: rotate_provider 移入 ops/snapshot.rs（供 AliyunDriver 复用）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: `AliyunDriver` 实现 6 个命令方法

**Files:**
- Modify: `src/cloud/aliyun/driver.rs`（在 `impl ProviderDriver for AliyunDriver` 中新增 6 个方法）

**Interfaces:**
- Consumes: `scan_regions`（Task 1）、`ops::snapshot::rotate_provider`（Task 4）、`AliyunProvider` / `domain::DomainClient`、`ops::{expiry, disk, ecs}`
- Produces: `AliyunDriver::{rotate, expiry, domain_expiry, disk, ecs_autosnapshot, ecs_expiry}` 完整实现

- [ ] **Step 1: 补 import**

在 `src/cloud/aliyun/driver.rs` 顶部 import 改为：

```rust
use crate::cloud::aliyun::{scan_regions, AliyunProvider};
use crate::cloud::driver::{Command, DiskGroup, DiskGroups, ProviderDriver};
use crate::config::{Config, Project, ProviderConfig};
use crate::ops;
use crate::ops::ecs::AutoSnapshotStatus;
use crate::ops::expiry::{DomainAlert, ExpiryAlert};
use anyhow::{anyhow, Result};
use chrono::Utc;
```

- [ ] **Step 2: 在 `impl ProviderDriver for AliyunDriver` 中追加 6 个方法**

在 `credentials` 方法之后、`impl` 块结束前插入：

```rust
    async fn rotate(
        &self,
        cfg: &Config,
        project: &Project,
        keep: usize,
        wait_minutes: u64,
    ) -> Result<()> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        ops::snapshot::rotate_provider(&provider, &cfg.notify, keep, wait_minutes).await
    }

    async fn expiry(
        &self,
        cfg: &Config,
        project: &Project,
        thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        ops::expiry::check(&provider, thresholds, Utc::now()).await
    }

    async fn domain_expiry(
        &self,
        cfg: &Config,
        project: &Project,
        thresholds: &[i64],
    ) -> Result<Vec<DomainAlert>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let client = crate::cloud::aliyun::domain::DomainClient::new(&ak, &sk);
        ops::expiry::check_domains(&client, thresholds, Utc::now()).await
    }

    async fn disk(
        &self,
        cfg: &Config,
        project: &Project,
        threshold: f64,
    ) -> Result<DiskGroups> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        let mut groups = Vec::new();

        // SWAS → label "aliyun"
        let scan = scan_regions(provider.groups(), "SWAS 磁盘检查", "SWAS", |g| {
            ops::disk::check_swas_disk(&g.swas, &g.region, threshold)
        })
        .await;
        let err = scan.all_failed_err("SWAS 磁盘检查");
        let (mut over, mut missing) = (Vec::new(), Vec::new());
        for (o, m) in scan.items {
            over.extend(o);
            missing.extend(m);
        }
        groups.push(DiskGroup {
            label: "aliyun".to_string(),
            over,
            missing,
            error: err.map(|e| format!("{e:#}")),
        });

        // ECS → label "aliyun-ecs"
        let scan = scan_regions(provider.groups(), "ECS 磁盘检查", "ECS", |g| {
            ops::disk::check_ecs_disk(&g.ecs, &g.cms, threshold)
        })
        .await;
        let err = scan.all_failed_err("ECS 磁盘检查");
        let (mut over, mut missing) = (Vec::new(), Vec::new());
        for (o, m) in scan.items {
            over.extend(o);
            missing.extend(m);
        }
        groups.push(DiskGroup {
            label: "aliyun-ecs".to_string(),
            over,
            missing,
            error: err.map(|e| format!("{e:#}")),
        });

        Ok(DiskGroups { groups })
    }

    async fn ecs_autosnapshot(
        &self,
        cfg: &Config,
        project: &Project,
    ) -> Result<Vec<AutoSnapshotStatus>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        let scan = scan_regions(provider.groups(), "ECS 自动快照检查", "ECS", |g| {
            ops::ecs::check_auto_snapshot(&g.ecs)
        })
        .await;
        if let Some(e) = scan.all_failed_err("ECS 自动快照检查") {
            return Err(e);
        }
        Ok(scan.items.into_iter().flatten().collect())
    }

    async fn ecs_expiry(
        &self,
        cfg: &Config,
        project: &Project,
        thresholds: &[i64],
    ) -> Result<Vec<ExpiryAlert>> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let provider = AliyunProvider::new(&ak, &sk, &pcfg.region).await?;
        let scan = scan_regions(provider.groups(), "ECS 查询", "ECS", |g| {
            g.ecs.list_servers()
        })
        .await;
        if let Some(e) = scan.all_failed_err("ECS 查询") {
            return Err(e);
        }
        let servers: Vec<_> = scan.items.into_iter().flatten().collect();
        Ok(ops::expiry::check_servers(servers, thresholds, Utc::now()))
    }
```

- [ ] **Step 3: 编译 + 回归**

Run: `cargo check` 与 `cargo test`
Expected: 编译通过，测试全绿。`resources` 仍用 trait 默认实现（Task 7 补齐）。

- [ ] **Step 4: 提交**

```bash
git add src/cloud/aliyun/driver.rs
git commit -m "feat: AliyunDriver 实现 6 个命令方法（内部复用 scan_regions）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: `main.rs` 改为驱动循环，删除 `run_provider_*`

**Files:**
- Modify: `src/main.rs`（删除 7 个 `run_provider_*` + `run_project_rotate` + `run_ecs_autosnapshot_project` + `provider_kinds`；命令臂改驱动循环；新增 `drivers_for_project`）

**Interfaces:**
- Consumes: `cloud::driver::{Command, ProviderDriver, driver, drivers, supported_kinds}`、`AliyunDriver`（经注册表）、`ops::{expiry, disk}`、`cloud::aliyun::is_permission_error`
- Produces: `fn drivers_for_project<'a>(cfg: &'a Config, project: &'a Project, filter: Option<&str>) -> anyhow::Result<Vec<&'a dyn ProviderDriver>>`
- Behavior（spec §3）：B3 未实现服务商宽容跳过；B5 表头行序微移；B6 各命令臂对 `drivers_for_project` 错误按项目记录（不再 `bail!` 整命令）

- [ ] **Step 1: 顶部 import 调整**

在 `src/main.rs` 顶部把 `use crate::cloud::CloudProvider;` 改为：

```rust
use crate::cloud::driver::{Command as DriverCommand, ProviderDriver};
```

（`CloudProvider` 不再被 main.rs 直接使用；`cloud` 模块本身通过 `mod cloud;` 引用，如 `cloud::aliyun::is_permission_error`。**注意**：`main.rs` 已有本地 `Command` 枚举（clap 子命令），故驱动命令类型别名导入为 `DriverCommand`，以下各臂一律用 `DriverCommand::X`。）

- [ ] **Step 2: 新增 `drivers_for_project`**

在 `parse_thresholds` 之后新增：

```rust
/// 项目内应执行的驱动列表：--provider 过滤 + 项目已配置校验。
///
/// - `--provider` 指定未注册服务商 → 报「尚未实现」
/// - `--provider` 指定项目未配置 → 报「未配置」
/// - 未指定：遍历项目已配置的服务商；未实现的服务商 → 打印警告并跳过（宽容）
fn drivers_for_project<'a>(
    cfg: &'a config::Config,
    project: &'a config::Project,
    filter: Option<&str>,
) -> anyhow::Result<Vec<&'a dyn ProviderDriver>> {
    use crate::cloud::driver::{driver, drivers, supported_kinds};

    match filter {
        Some(k) => {
            if driver(k).is_none() {
                anyhow::bail!(
                    "服务商 {k:?} 尚未实现（目前仅支持: {}）",
                    supported_kinds().join(", ")
                );
            }
            if !project.providers.contains_key(k) {
                anyhow::bail!(
                    "项目 {} 未配置服务商 {k:?}（可用: {}）",
                    project.name,
                    project.providers.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(vec![driver(k).unwrap()])
        }
        None => {
            let mut out = Vec::new();
            for k in project.providers.keys() {
                match driver(k) {
                    Some(d) => out.push(d),
                    None => println!(
                        "  跳过未实现的服务商: {k}（当前仅支持: {}）",
                        supported_kinds().join(", ")
                    ),
                }
            }
            Ok(out)
        }
    }
}
```

把以下测试加入 `src/main.rs` 的 `mod tests`（现有 `use super::*; use clap::Parser;` 之后）：

```rust
    #[test]
    fn test_drivers_for_project() {
        use crate::config::{Config, Project, ProviderConfig};
        use std::collections::BTreeMap;

        let project = Project {
            name: "demo".into(),
            description: None,
            providers: BTreeMap::from([
                ("aliyun".into(), ProviderConfig::default()),
                ("tencent".into(), ProviderConfig::default()),
            ]),
        };
        let cfg = Config {
            projects: vec![project],
            notify: Default::default(),
        };
        let p = &cfg.projects[0];

        // --provider aliyun（已配置）→ 命中
        let ds = drivers_for_project(&cfg, p, Some("aliyun")).unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].kind(), "aliyun");

        // --provider tencent（未实现）→ 报「尚未实现」
        let err = drivers_for_project(&cfg, p, Some("tencent")).unwrap_err();
        assert!(err.to_string().contains("尚未实现"));

        // --provider 已实现但项目未配置 → 报「未配置」
        let cfg2 = Config {
            projects: vec![Project {
                name: "other".into(),
                description: None,
                providers: BTreeMap::new(),
            }],
            notify: Default::default(),
        };
        let err = drivers_for_project(&cfg2, &cfg2.projects[0], Some("aliyun")).unwrap_err();
        assert!(err.to_string().contains("未配置"));

        // 未指定 --provider → 只返回已实现服务商（tencent 打印警告并跳过）
        let ds = drivers_for_project(&cfg, p, None).unwrap();
        let kinds: Vec<&str> = ds.iter().map(|d| d.kind()).collect();
        assert_eq!(kinds, vec!["aliyun"]);
    }
```

- [ ] **Step 3: 运行 `drivers_for_project` 测试确认通过**

Run: `cargo test drivers_for_project -- --nocapture`
Expected: PASS（无失败测试先行的理由：该 helper 是纯搬运的查询逻辑，行为由 spec §3 锁定；测试直接验证结果）。

- [ ] **Step 4: 重写 `Command::Snapshot` 臂**

把 `main()` 中 `Command::Snapshot { keep, wait_minutes } => { ... }` 整体替换为：

```rust
        Command::Snapshot { keep, wait_minutes } => {
            let targets = select_projects(&cfg, cli.project.as_deref())?;
            let mut errors = Vec::new();
            for project in targets {
                println!("\n===== 项目: {} =====", project.name);
                let drivers = match drivers_for_project(&cfg, project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 执行失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                for driver in drivers {
                    println!("-- 服务商: {}", driver.kind());
                    if driver.commands().contains(&DriverCommand::Snapshot) {
                        if let Err(e) = driver.rotate(&cfg, project, keep, wait_minutes).await {
                            println!("项目 {} 执行失败: {e:#}", project.name);
                            errors.push(project.name.clone());
                        }
                    }
                    if driver.commands().contains(&DriverCommand::EcsAutosnapshot) {
                        if let Err(e) = driver.ecs_autosnapshot(&cfg, project).await {
                            println!("项目 {} ECS 自动快照检查失败: {e:#}", project.name);
                            errors.push(format!("{} (ECS 检查)", project.name));
                        }
                    }
                }
            }
            errors
        }
```

- [ ] **Step 5: 重写 `Command::Expiry` 臂**

替换为（汇总通知与无命中分支保持原样，仅替换收集循环）：

```rust
        Command::Expiry { days } => {
            let thresholds = parse_thresholds(&days)?;

            let targets = select_projects(&cfg, cli.project.as_deref())?;

            // 汇总全部项目/服务商的命中提醒，最后发一条通知（避免刷屏）
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut alerts: Vec<(String, String, ops::expiry::ExpiryAlert)> = Vec::new();
            let mut errors = Vec::new();
            for project in &targets {
                println!("\n===== 项目: {} =====", project.name);
                let drivers = match drivers_for_project(&cfg, project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 检查失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                for driver in drivers {
                    if driver.commands().contains(&DriverCommand::Expiry) {
                        println!("-- 服务商: {}", driver.kind());
                        match driver.expiry(&cfg, project, &thresholds).await {
                            Ok(list) => alerts.extend(list.into_iter().map(|a| {
                                (project.name.clone(), driver.kind().to_string(), a)
                            })),
                            Err(e) => {
                                println!("服务商 {} 检查失败: {e:#}", driver.kind());
                                errors.push(driver.kind().to_string());
                            }
                        }
                    }
                    if driver.commands().contains(&DriverCommand::EcsExpiry) {
                        println!("-- ECS 到期检查");
                        let label = format!("{}-ecs", driver.kind());
                        match driver.ecs_expiry(&cfg, project, &thresholds).await {
                            Ok(list) => alerts.extend(list.into_iter().map(|a| {
                                (project.name.clone(), label.clone(), a)
                            })),
                            Err(e) => {
                                println!("ECS 到期检查失败: {e:#}");
                                errors.push(label.clone());
                            }
                        }
                    }
                }
            }

            if !alerts.is_empty() {
                let text = ops::expiry::render(&alerts);
                println!("{text}");
                if let Some(n) = &notifier {
                    let title = format!("服务器到期提醒: {} 台需关注", alerts.len());
                    if let Err(e) = n.send(&title, &text).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            } else {
                let list = thresholds
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("全部实例均在安全期内（{list} 天内无到期）");
            }

            if !errors.is_empty() {
                anyhow::bail!("以下服务商检查失败: {}", errors.join(", "));
            }
            Vec::new()
        }
```

- [ ] **Step 6: 重写 `Command::ExpiryDomain` 臂**

替换为（渲染/通知/无命中分支保持原样）：

```rust
        Command::ExpiryDomain { days } => {
            let thresholds = parse_thresholds(&days)?;
            let targets = select_projects(&cfg, cli.project.as_deref())?;

            // 域名是账号级全局资源，每个项目查一次（不受地域影响）
            let notifier = crate::notify::from_config(&cfg.notify)?;
            let mut alerts: Vec<(String, String, ops::expiry::DomainAlert)> = Vec::new();
            let mut errors = Vec::new();
            for project in &targets {
                println!("\n===== 项目: {} =====", project.name);
                let drivers = match drivers_for_project(&cfg, project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 域名检查失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                for driver in drivers {
                    if !driver.commands().contains(&DriverCommand::ExpiryDomain) {
                        continue;
                    }
                    match driver.domain_expiry(&cfg, project, &thresholds).await {
                        Ok(list) => alerts.extend(list.into_iter().map(|a| {
                            (project.name.clone(), driver.kind().to_string(), a)
                        })),
                        Err(e) => {
                            if cloud::aliyun::is_permission_error(&e) {
                                // 账号未开通/未授权域名服务（如无域名资源）→ 跳过，不视为失败
                                println!("  跳过域名检查（无 domain 权限，可能未注册域名）");
                            } else {
                                println!("域名到期检查失败: {e:#}");
                                errors.push(driver.kind().to_string());
                            }
                        }
                    }
                }
            }

            if !alerts.is_empty() {
                let text = ops::expiry::render_domains(&alerts);
                println!("{text}");
                if let Some(n) = &notifier {
                    let title = format!("域名到期提醒: {} 个需关注", alerts.len());
                    if let Err(e) = n.send(&title, &text).await {
                        tracing::warn!("通知发送失败: {e}");
                    }
                }
            } else {
                let list = thresholds
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("全部域名均在安全期内（{list} 天内无到期）");
            }

            if !errors.is_empty() {
                anyhow::bail!("以下项目域名检查失败: {}", errors.join(", "));
            }
            Vec::new()
        }
```

- [ ] **Step 7: 重写 `Command::Disk` 臂**

替换为（渲染/通知分支保持原样）：

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
                let drivers = match drivers_for_project(&cfg, project, cli.provider.as_deref()) {
                    Ok(d) => d,
                    Err(e) => {
                        println!("项目 {} 磁盘检查失败: {e:#}", project.name);
                        errors.push(project.name.clone());
                        continue;
                    }
                };
                for driver in drivers {
                    if !driver.commands().contains(&DriverCommand::Disk) {
                        continue;
                    }
                    match driver.disk(&cfg, project, threshold).await {
                        Ok(groups) => {
                            for g in groups.groups {
                                if let Some(err) = g.error {
                                    println!("服务商 {} 磁盘检查失败: {err}", g.label);
                                    errors.push(g.label.clone());
                                    continue;
                                }
                                over.extend(g.over.into_iter().map(|s| {
                                    (project.name.clone(), g.label.clone(), s)
                                }));
                                missing.extend(g.missing.into_iter().map(|s| {
                                    (project.name.clone(), g.label.clone(), s)
                                }));
                            }
                        }
                        Err(e) => {
                            println!("服务商 {} 磁盘检查失败: {e:#}", driver.kind());
                            errors.push(driver.kind().to_string());
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

- [ ] **Step 8: 删除旧函数**

在 `src/main.rs` 中删除以下整块函数：`run_project_rotate`、`run_provider_rotate`、`run_provider_expiry`、`run_provider_disk_swas`、`run_provider_domain_expiry`、`run_provider_disk_ecs`、`run_ecs_autosnapshot_project`、`run_provider_ecs_autosnapshot`、`run_provider_ecs_expiry`、`provider_kinds`。（`rotate_provider` 已在 Task 4 删除。）

- [ ] **Step 8: 编译 + 回归**

Run: `cargo check` 与 `cargo test`
Expected: 编译通过，测试全绿（含 `main.rs::tests::test_config_global_accepts_both_positions`）。

- [ ] **Step 9: 提交**

```bash
git add src/main.rs
git commit -m "refactor: main.rs 命令臂改为 ProviderDriver 驱动循环，删除全部 run_provider_*

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: `serve/api.rs` 走注册表 + `AliyunDriver::resources` + 清理

**Files:**
- Modify: `src/cloud/aliyun/driver.rs`（新增 `resources()` 方法 + `ResourceItem`/`ResourceGroup` + `collect_*` 助手）
- Modify: `src/serve/api.rs`（`gather_resources` 走注册表；`providers()` 改 `supported_kinds()`；删除本地 `collect_*`/`ResourceGroup`/`ResourceItem`）
- Modify: `src/cloud/mod.rs`（删除 `SUPPORTED_PROVIDERS`）
- Modify: `src/config.rs`（删除 `ProviderConfig::aliyun_credentials`）

**Interfaces:**
- Produces: `AliyunDriver::resources(&self, cfg: &Config, project: &Project) -> anyhow::Result<serde_json::Value>`（返回 `{ swas: ResourceGroup, ecs: ResourceGroup, domains: ResourceGroup }` JSON）
- Behavior：`GET /api/providers` 返回结构不变（`{ "providers": ["aliyun"] }`）；`gather_resources` 输出结构不变

- [ ] **Step 1: `cloud/aliyun/driver.rs` 补资源助手 + `resources()`**

在 `src/cloud/aliyun/driver.rs` 顶部追加 import：

```rust
use crate::cloud::Server;
use crate::ops::expiry;
```

在文件末尾（`mod tests` 之外）追加：

```rust
/// 资源条目（SWAS / ECS / 域名统一结构；auto_renew 仅域名使用）
#[derive(serde::Serialize)]
pub struct ResourceItem {
    pub id: String,
    pub name: String,
    pub region: String,
    pub status: String,
    pub expired_at: Option<String>,
    pub days_left: Option<i64>,
    pub auto_renew: bool,
}

#[derive(serde::Serialize)]
pub struct ResourceGroup {
    pub ok: bool,
    pub error: Option<String>,
    pub items: Vec<ResourceItem>,
}

fn resource_item(server: Server) -> ResourceItem {
    let days_left = server
        .expired_at
        .map(|t| expiry::days_left(t, chrono::Utc::now()));
    ResourceItem {
        id: server.id,
        name: server.name,
        region: server.region,
        status: server.status,
        expired_at: server.expired_at.map(|t| t.to_rfc3339()),
        days_left,
        auto_renew: false,
    }
}

fn resource_error(msg: String) -> ResourceGroup {
    ResourceGroup {
        ok: false,
        error: Some(msg),
        items: Vec::new(),
    }
}

/// SWAS 实例资源（global 模式跨地域汇总已内聚于 AliyunProvider::list_servers）。
async fn collect_swas(ak: &str, sk: &str, region: &str) -> ResourceGroup {
    use crate::cloud::CloudProvider;

    match AliyunProvider::new(ak, sk, region).await {
        Ok(p) => match p.list_servers().await {
            Ok(servers) => ResourceGroup {
                ok: true,
                error: None,
                items: servers.into_iter().map(resource_item).collect(),
            },
            Err(e) => resource_error(format!("{e:#}")),
        },
        Err(e) => resource_error(format!("{e:#}")),
    }
}

/// ECS 实例资源：跨地域遍历（权限类错误跳过；非权限错误记录地域）。
async fn collect_ecs(ak: &str, sk: &str, region: &str) -> ResourceGroup {
    use crate::cloud::aliyun::is_permission_error;

    match AliyunProvider::new(ak, sk, region).await {
        Ok(p) => {
            let mut items = Vec::new();
            let mut errs: Vec<String> = Vec::new();
            let mut no_perm = 0;
            for g in p.groups() {
                match g.ecs.list_servers().await {
                    Ok(s) => items.extend(s.into_iter().map(resource_item)),
                    Err(e) if is_permission_error(&e) => no_perm += 1,
                    Err(e) => errs.push(format!("{}: {e:#}", g.region)),
                }
            }
            let error = if !errs.is_empty() {
                Some(format!("部分地域查询失败: {}", errs.join("; ")))
            } else if items.is_empty() && no_perm > 0 {
                Some(format!("{} 个地域无 ECS 权限（可能未购买/未授权）", no_perm))
            } else {
                None
            };
            ResourceGroup {
                ok: true,
                error,
                items,
            }
        }
        Err(e) => resource_error(format!("{e:#}")),
    }
}

/// 域名资源（账号级全局服务）；权限类错误视为"未注册域名"跳过（不标失败）。
async fn collect_domains(ak: &str, sk: &str) -> ResourceGroup {
    use crate::cloud::aliyun::is_permission_error;

    match crate::cloud::aliyun::domain::DomainClient::new(ak, sk)
        .list_domains()
        .await
    {
        Ok(ds) => ResourceGroup {
            ok: true,
            error: None,
            items: ds
                .into_iter()
                .map(|d| {
                    let days_left = d
                        .expired_at
                        .map(|t| expiry::days_left(t, chrono::Utc::now()));
                    ResourceItem {
                        id: d.domain_name.clone(),
                        name: d.domain_name,
                        region: "全局".to_string(),
                        status: String::new(),
                        expired_at: d.expired_at.map(|t| t.to_rfc3339()),
                        days_left,
                        auto_renew: d.auto_renew,
                    }
                })
                .collect(),
        },
        Err(e) if is_permission_error(&e) => ResourceGroup {
            ok: true,
            error: Some("无 domain 权限（可能未注册域名）".to_string()),
            items: Vec::new(),
        },
        Err(e) => resource_error(format!("{e:#}")),
    }
}
```

在 `impl ProviderDriver for AliyunDriver` 内、`ecs_expiry` 之后追加：

```rust
    async fn resources(
        &self,
        cfg: &Config,
        project: &Project,
    ) -> Result<serde_json::Value> {
        let pcfg = cfg.provider(project, self.kind())?;
        let (ak, sk) = self.credentials(pcfg)?;
        let region = pcfg.region.clone();
        let mut out = serde_json::Map::new();
        out.insert(
            "swas".to_string(),
            serde_json::to_value(collect_swas(&ak, &sk, &region).await)?,
        );
        out.insert(
            "ecs".to_string(),
            serde_json::to_value(collect_ecs(&ak, &sk, &region).await)?,
        );
        out.insert(
            "domains".to_string(),
            serde_json::to_value(collect_domains(&ak, &sk).await)?,
        );
        Ok(serde_json::Value::Object(out))
    }
```

- [ ] **Step 2: 重写 `serve/api.rs` 的 `gather_resources`**

把 `src/serve/api.rs` 中的 `gather_resources`（约 452-528 行）整体替换为：

```rust
/// 收集全部配置了已实现服务商（且有凭据）项目的资源，返回 `projects` 数组（JSON）。
/// 任务粒度 = 项目 × 驱动，全局并发上限 [`RESOURCE_CONCURRENCY`]。
/// 供启动预取 / 配置变更刷新 / 手动刷新共用。
pub async fn gather_resources(config_dir: &Path) -> anyhow::Result<serde_json::Value> {
    use crate::cloud::driver::{drivers, ProviderDriver};
    use std::collections::BTreeMap;
    use tokio::sync::Semaphore;

    const RESOURCE_CONCURRENCY: usize = 4;

    let (projects, notify) = read_config_files(config_dir)?;
    let cfg = config::Config {
        projects: projects.clone(),
        notify,
    };

    // 收集任务列表：项目 × 已配置且凭据完整的驱动（未配置凭据的项目跳过）
    let mut tasks = Vec::new();
    for p in &projects {
        for d in drivers() {
            if !p.providers.contains_key(d.kind()) {
                continue;
            }
            let pcfg = &p.providers[d.kind()];
            if d.credentials(pcfg).is_err() {
                continue;
            }
            tasks.push((p.clone(), *d));
        }
    }

    // 并发执行（信号量限流），按项目聚合
    let sem = std::sync::Arc::new(Semaphore::new(RESOURCE_CONCURRENCY));
    let mut handles = Vec::new();
    for (project, driver) in tasks {
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("资源信号量");
            let value = match driver.resources(&cfg, &project).await {
                Ok(v) => v,
                Err(e) => {
                    serde_json::json!({ "ok": false, "error": format!("{e:#}"), "items": [] })
                }
            };
            (
                project.name,
                project.description.unwrap_or_default(),
                driver.kind().to_string(),
                value,
            )
        }));
    }

    let mut map: BTreeMap<String, serde_json::Map<String, serde_json::Value>> = BTreeMap::new();
    for h in handles {
        let (name, description, kind, value) =
            h.await.map_err(|e| anyhow::anyhow!("资源收集任务异常: {e}"))?;
        let entry = map.entry(name.clone()).or_default();
        entry.insert("name".to_string(), serde_json::json!(name));
        entry.insert("description".to_string(), serde_json::json!(description));
        // 结构：{ name, providers: { <provider_kind>: <driver.resources JSON> } }
        let providers = entry
            .entry("providers".to_string())
            .or_insert_with(|| serde_json::json!({}));
        providers
            .as_object_mut()
            .expect("providers 应为对象")
            .insert(kind, value);
    }
    let projects: Vec<serde_json::Value> =
        map.into_values().map(serde_json::Value::Object).collect();
    Ok(serde_json::json!({ "projects": projects }))
}
```

- [ ] **Step 3: 删除 `api.rs` 中已移走的资源助手**

在 `src/serve/api.rs` 中删除：`ResourceItem`、`ResourceGroup`、`ResourceCache` 的 `resource_item`、`resource_error`、`collect_swas`、`collect_ecs`、`collect_domains`（结构体 `ResourceItem`/`ResourceGroup` 与其助手函数；`ResourceCache` 本身保留）。同时删除 `gather_resources` 内的 `use crate::cloud::aliyun::{is_permission_error, AliyunProvider};` 与 `use crate::cloud::CloudProvider;`（若随删除代码一并消失）。

- [ ] **Step 4: `providers()` 改读注册表**

把 `src/serve/api.rs` 的 `pub async fn providers()` 替换为：

```rust
pub async fn providers() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "providers": crate::cloud::driver::supported_kinds() }))
}
```

- [ ] **Step 5: `cloud/mod.rs` 删除 `SUPPORTED_PROVIDERS`**

删除 `src/cloud/mod.rs` 中：

```rust
/// 当前已接入的云服务商 kind 列表（唯一权威来源）。
/// 前端配置页“添加服务商”下拉、资源页 provider Tab 均从此处获取；
/// 接入新服务商（tencent/aws…）时在此追加，前端无需改动。
pub const SUPPORTED_PROVIDERS: &[&str] = &["aliyun"];
```

- [ ] **Step 6: `config.rs` 删除 `aliyun_credentials`**

删除 `src/config.rs` 中整个：

```rust
impl ProviderConfig {
    pub fn aliyun_credentials(&self) -> Result<(String, String)> {
        // ...（约 173-195 行）
    }
}
```

确认后无其它引用（main.rs 已用 `driver.credentials`；api.rs 已用 `d.credentials(pcfg)`）。

- [ ] **Step 7: 编译 + 回归**

Run: `cargo check` 与 `cargo test`
Expected: 编译通过，测试全绿（`api.rs` 的 `test_config_get_masks_secret` 等不受影响；`providers()` 无直接测试）。

- [ ] **Step 8: 提交**

```bash
git add src/cloud/aliyun/driver.rs src/serve/api.rs src/cloud/mod.rs src/config.rs
git commit -m "refactor: gather_resources 走驱动注册表；删除 SUPPORTED_PROVIDERS 与 config.aliyun_credentials

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 收尾验证

全部任务完成后，整体验证一次：

- [ ] `cargo check` — 无 warning
- [ ] `cargo test` — 全部测试通过（新增测试约 12 个 + 既有 66 个）
- [ ] `cargo build --release` — 发布构建通过
- [ ] 手工冒烟（可选，需真实凭据，**不自动执行**）：`ops-console projects` 应正常列出项目。
