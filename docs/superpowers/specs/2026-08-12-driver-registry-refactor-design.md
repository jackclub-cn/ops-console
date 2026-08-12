# Provider Driver 注册表 + 跨地域巡检消重 重构设计

日期：2026-08-12
范围：CLI 侧 `main.rs` 分发逻辑消重 + 服务商接入收敛为「一处注册」；Web 资源列表一并纳入。
前提：现有 66 个测试全部保持通过；CLI 可观察输出（摘要 / 通知 / 错误消息 / 退出码）保持语义不变。

## 1. 现状问题

### 1.1 跨地域巡检重复（第 1 点）

`main.rs` 中以下函数共享同一段「权限跳过 → 失败汇总 → 全部失败才报错」骨架，各约 40-50 行，几乎逐字重复：

- `run_provider_disk_swas`（466-518）
- `run_provider_disk_ecs`（539-590）
- `run_provider_ecs_autosnapshot`（689-735）
- `run_provider_ecs_expiry`（738-785）

`cloud/aliyun/mod.rs::AliyunProvider::list_servers`（190-252）为第 5 处，结构相同但用 `tracing::warn` 记录跳过。

### 1.2 服务商接入分散（第 2 点）

接入一个新服务商需同步 4 处：

| 位置 | 现状 |
|---|---|
| `cloud/mod.rs` | `SUPPORTED_PROVIDERS = &["aliyun"]` |
| `main.rs` | 7 个 `run_provider_*` 各自的 `match kind { "aliyun" => ..., other => bail!(...) }` |
| `config.rs` | `ProviderConfig::aliyun_credentials()` 硬编码凭据解析 |
| `serve/api.rs` | `gather_resources` 硬编码 `aliyun` + `["swas","ecs","domains"]` |

## 2. 设计

### 2.1 `scan_regions`：跨地域巡检 helper（放 `cloud/aliyun/mod.rs`）

```rust
pub struct RegionScan<T> {
    pub items: Vec<T>,                   // 每个成功地域的结果（T 由闭包决定，可为元组）
    pub no_perm: Vec<String>,            // 权限类错误跳过的地域
    pub failed: Vec<(String, String)>,   // (地域, 错误信息) 非权限失败
}

pub async fn scan_regions<T, F, Fut>(
    groups: &[RegionGroup],
    label: &str,    // 如 "SWAS 磁盘检查"，用于失败跳过提示
    product: &str,  // 如 "SWAS"，用于权限跳过提示
    f: F,           // Fn(&RegionGroup) -> Fut, Fut: Future<Output = anyhow::Result<T>>
) -> RegionScan<T>;

impl<T> RegionScan<T> {
    /// 全部地域失败且无任何产出与权限跳过 → Some(总失败错误)；else None。
    /// 单资源族命令调用方据此返回 Err；磁盘双资源族命令按族分别记录。
    pub fn all_failed_err(&self, label: &str) -> Option<anyhow::Error>;
}
```

行为：

- 权限类错误（`is_permission_error`）→ 记入 `no_perm`，打印 `跳过 {n} 个地域（无 {product} 权限，可能未购买/未授权）: ...`
- 非权限错误 → 记入 `failed`，打印 `跳过 {n} 个地域（{label}失败）: ...`
- **`all_failed_err` 判定**：`!failed.is_empty() && items.is_empty() && no_perm.is_empty()`。错误消息含首个错误，与现状一致。

**行为改进（有意）**：`all_failed` 判定从「合并结果为空」改为「无任何地域成功返回」。旧逻辑中"地域 A 正常但全部实例健康（空结果）+ 地域 B 网络失败"会被误判为总失败；新逻辑更准确（成功返回过即证明 API 通）。错误消息与退出码随之略降。

`list_servers` 一并改用 `scan_regions`，跳过提示从 `tracing::warn` 改为 `println`（与其它地域跳过提示统一，对 CLI 用户更可见）。

### 2.2 `ProviderDriver` trait + 注册表（新文件 `cloud/driver.rs`）

```rust
pub enum Command { Snapshot, Expiry, ExpiryDomain, Disk, EcsAutosnapshot, EcsExpiry }

pub struct DiskGroup {
    pub label: String,             // "aliyun" | "aliyun-ecs"（由驱动决定）
    pub over: Vec<DiskStatus>,
    pub missing: Vec<DiskStatus>,
    pub error: Option<String>,     // 该资源族全部地域失败时 Some
}
pub struct DiskGroups { pub groups: Vec<DiskGroup> }

#[async_trait::async_trait]
pub trait ProviderDriver: Send + Sync {
    fn kind(&self) -> &'static str;
    /// 能力声明：默认不支持任何命令；未声明的命令 main 层不调用
    fn commands(&self) -> &[Command] { &[] }
    /// 凭据解析：默认读 access_key_id / access_key_secret，缺省按 kind 提示环境变量
    fn credentials(&self, pcfg: &ProviderConfig) -> anyhow::Result<(String, String)>;
    // 每命令一方法，默认 bail!("服务商 {} 不支持 XXX", self.kind())：
    async fn rotate(&self, cfg: &Config, project: &Project, keep: usize, wait_minutes: u64) -> anyhow::Result<()>;
    async fn expiry(&self, cfg: &Config, project: &Project, thresholds: &[i64]) -> anyhow::Result<Vec<ExpiryAlert>>;
    async fn domain_expiry(&self, cfg: &Config, project: &Project, thresholds: &[i64]) -> anyhow::Result<Vec<DomainAlert>>;
    async fn disk(&self, cfg: &Config, project: &Project, threshold: f64) -> anyhow::Result<DiskGroups>;
    async fn ecs_autosnapshot(&self, cfg: &Config, project: &Project) -> anyhow::Result<Vec<AutoSnapshotStatus>>;
    async fn ecs_expiry(&self, cfg: &Config, project: &Project, thresholds: &[i64]) -> anyhow::Result<Vec<ExpiryAlert>>;
    async fn resources(&self, cfg: &Config, project: &Project) -> anyhow::Result<serde_json::Value>;
}

pub fn drivers() -> &'static [&'static dyn ProviderDriver];      // 唯一注册表
pub fn driver(kind: &str) -> Option<&'static dyn ProviderDriver>;
pub fn supported_kinds() -> Vec<&'static str>;                   // 取代 SUPPORTED_PROVIDERS
```

- 全部方法签名取 `&Config, &Project`（含通知配置 `cfg.notify` 与项目内 provider 配置）。
- **`rotate` 的通用实现 `rotate_provider`**（现于 `main.rs`，泛型于 `CloudProvider`）移至 `ops/snapshot.rs`，驱动内调用。

### 2.3 `AliyunDriver`（新文件 `cloud/aliyun/driver.rs`）

```rust
pub struct AliyunDriver;
```

实现全部 7 个方法 + `commands()`（6 个全部支持）+ `credentials()`（平移自 `config::ProviderConfig::aliyun_credentials`）。内部一律 `scan_regions` 消重：

- `rotate`：构造 `AliyunProvider` → `ops::snapshot::rotate_provider`
- `expiry`：构造 `AliyunProvider` → `ops::expiry::check`
- `domain_expiry`：构造 `DomainClient` → `ops::expiry::check_domains`
- `disk`：SWAS `check_swas_disk` → group(label="aliyun")；ECS `check_ecs_disk` → group(label="aliyun-ecs")；各族 `all_failed_err` 记入 `group.error`
- `ecs_autosnapshot`：`ops::ecs::check_auto_snapshot`，全失败 → Err
- `ecs_expiry`：`ecs.list_servers` 跨地域汇总 → `ops::expiry::check_servers`，全失败 → Err
- `resources`：平移 `api.rs` 的 `collect_swas / collect_ecs / collect_domains` + `ResourceGroup / ResourceItem` 结构，返回 `{ swas: {...}, ecs: {...}, domains: {...} }` JSON

**labels**：`disk()` 的 `"aliyun"` / `"aliyun-ecs"` 由驱动在产出点决定（SWAS 与 ECS 数据在驱动内合并，无法在 main 层区分）。`ecs_expiry` 的 label 在 main 层用 `format!("{}-ecs", driver.kind())` 拼，精确复刻现状并自动适配新服务商。

### 2.4 `main.rs` 重写

删除 `run_provider_*`（7 个）与 `run_project_rotate` / `run_ecs_autosnapshot_project` / `provider_kinds`。每个命令臂改为驱动循环：

```rust
fn drivers_for_project<'a>(cfg: &'a Config, project: &'a Project, filter: Option<&str>)
    -> anyhow::Result<Vec<&'a dyn ProviderDriver>>
```

职责：`--provider` 过滤（未注册 → 报「服务商 X 尚未实现（目前仅支持: ...）」；项目未配置 → 报「项目 X 未配置服务商 X」）；未指定时遍历已注册驱动，**项目配置了未实现服务商 → 打印警告并跳过**（宽容，见 §3）。

各命令臂：

- **Snapshot**：每项目 × 每驱动 → `driver.rotate`（错误记 project）→ 若 `commands` 含 EcsAutosnapshot 则 `driver.ecs_autosnapshot`（错误记 `{project} (ECS 检查)`）。
- **Expiry**：每项目 × 每驱动 → `driver.expiry`（label=kind，错误记 kind）+ 若含 EcsExpiry 则 `driver.ecs_expiry`（label=`{kind}-ecs`，错误记 `{kind}-ecs`）；全部汇总为一条通知（保持现状）。
- **ExpiryDomain**：每项目 × 每驱动 → `driver.domain_expiry`；权限类错误 → 打印「跳过域名检查（无 domain 权限...）」不报错，否则记错。
- **Disk**：每项目 × 每驱动 → `driver.disk`；每个 group：`error` 非空 → 打印「服务商 {label} 磁盘检查失败: {err}」并记 label 错误；否则 over/missing 并入（label=group.label）。
- **Projects**：保持现状（纯配置列举，不用驱动）。

保留 `parse_thresholds`、`select_projects`。

### 2.5 `serve/api.rs`：`gather_resources` 走注册表

```rust
for project in &projects {
    for driver in cloud::driver::drivers() {
        if !project.providers.contains_key(driver.kind()) { continue; }
        let pcfg = &project.providers[driver.kind()];
        if driver.credentials(pcfg).is_err() { continue; }   // 未配置凭据的项目跳过（与旧行为一致）
        match driver.resources(&cfg, project).await { Ok(v) => prov.insert(kind, v), Err(e) => ... }
    }
}
```

`cfg` 由 `read_config_files` 结果临时构造。`GET /api/providers` 改读 `cloud::driver::supported_kinds()`。

并发粒度变化：从「每项目 × 3 类任务」改为「每项目 1 个 `driver.resources` 任务」，信号量上限 `RESOURCE_CONCURRENCY=4` 不变（驱动内部三类查询顺序执行；单个项目查询多了一次串行，但总并发仍由信号量约束，属可接受权衡）。

### 2.6 `config.rs`

删除 `ProviderConfig::aliyun_credentials()`（逻辑移入 `AliyunDriver::credentials`）。`ProviderConfig` 字段（`region` / `access_key_id` / `access_key_secret`）保留（命名已通用）。环境变量覆盖（`apply_env_overrides`，仅写 `aliyun` provider）与 `REGION_GLOBAL` 保持不变。

### 2.7 `cloud/mod.rs`

`SUPPORTED_PROVIDERS` 常量移除，暴露 `pub use driver::supported_kinds`（或等价函数）。`server`/`snapshot` 模型与 `CloudProvider` trait 不变。

## 3. 行为变更清单

| # | 变更 | 说明 |
|---|---|---|
| B1 | `all_failed` 判定语义 | 「无任何地域成功」而非「合并结果为空」；总失败误报略降（见 §2.1） |
| B2 | `list_servers` 跳过提示 | `tracing::warn` → `println`，与其它地域跳过提示统一 |
| B3 | 未实现服务商配置 | `--provider` 显式指定 → 仍报「尚未实现」；未指定 → 打印警告并跳过，不再硬失败 |
| B4 | `gather_resources` 并发粒度 | 每项目 1 任务（驱动内部三类顺序），信号量上限不变 |
| B5 | CLI 输出行序 | 关键摘要/通知/错误保留；个别结构性表头行序可能微移（语义不变） |

## 4. 测试

新增：

1. `scan_regions`：全成功 / 部分权限跳过 / 部分真实失败 / 全失败 / 全失败但含权限跳过（不视为总失败）/ 空结果地域算成功。
2. `RegionScan::all_failed_err` 各分支。
3. 注册表：`driver("aliyun")` 命中、`driver("tencent")` 为 None、`supported_kinds() == ["aliyun"]`、`drivers()` 返回注册列表。
4. `AliyunDriver`：`commands()` 含全部 6 个命令；`credentials` 缺凭据时报错、有凭据时解析正确。
5. `drivers_for_project`（main.rs）：`--provider` 未注册报错、项目未配置报错、未指定时跳过未实现服务商。

现有 66 个测试应全部保持通过（`api.rs` / `tasks.rs` / `config.rs` 测试不触碰 `run_provider_*` 与驱动路径；`gather_resources` 无直接测试）。

## 5. 文件改动清单

| 文件 | 改动 |
|---|---|
| `src/cloud/driver.rs` | 新增：`Command` / `DiskGroup` / `DiskGroups` / `ProviderDriver` trait / `drivers()` / `driver()` / `supported_kinds()` |
| `src/cloud/aliyun/driver.rs` | 新增：`AliyunDriver` 实现 |
| `src/cloud/aliyun/mod.rs` | 新增 `scan_regions` / `RegionScan`；`list_servers` 改用之 |
| `src/cloud/mod.rs` | 移除 `SUPPORTED_PROVIDERS`；暴露 `supported_kinds`；声明 `driver` 模块 |
| `src/cloud/aliyun/mod.rs` | 声明 `driver` 子模块 |
| `src/ops/snapshot.rs` | 移入通用 `rotate_provider`（自 main.rs） |
| `src/main.rs` | 删除 7 个 `run_provider_*` 等；命令臂改驱动循环；新增 `drivers_for_project` |
| `src/config.rs` | 删除 `aliyun_credentials()` |
| `src/serve/api.rs` | `gather_resources` 走注册表；`providers()` 读 `supported_kinds()`；`ResourceGroup`/`collect_*` 移出 |

## 6. 不做（Out of scope）

- `cloud::CloudProvider` trait / `Server` / `Snapshot` 模型不变。
- 环境变量覆盖 `apply_env_overrides` 保持 aliyun 专用，不泛化。
- `config::ProviderConfig` 不改为任意字段映射（`BTreeMap<String, Value>`），新服务商若字段不同需自扩（未来项）。
- Web 前端不改（`GET /api/providers` 返回结构不变）。
