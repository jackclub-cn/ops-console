//! 阿里云 provider 实现。
//!
//! region 支持两种模式：
//! - 具体地域（如 `cn-shenzhen`）：单地域，行为与旧版一致
//! - 留空 / `global`：自动发现账号下全部地域（SWAS ListRegions ∪ ECS DescribeRegions），
//!   跨地域汇总巡检。适合"一个项目 = 一个阿里云运维账号"的全量运维场景。

pub mod cms;
pub mod domain;
pub mod ecs;
pub mod resourcecenter;
pub mod rpc;
pub mod sign;
pub mod swas;

use crate::cloud::{CloudProvider, Server, Snapshot, SnapshotStatus};
use crate::config::REGION_GLOBAL;
use anyhow::{anyhow, Result};
use std::time::Duration;

use self::rpc::parse_expired_time;

/// 全局接口（SWAS ListRegions / ECS DescribeRegions）使用的引导地域；
/// 这两个接口返回与调用地域无关的全局结果，地域仅用于拼接接入点。
const DISCOVERY_REGION: &str = "cn-hangzhou";

/// 一个地域的完整客户端组（SWAS + ECS + 云监控）
#[derive(Debug, Clone)]
pub struct RegionGroup {
    pub region: String,
    pub swas: swas::SwasClient,
    pub ecs: ecs::EcsClient,
    pub cms: cms::CmsClient,
}

impl RegionGroup {
    fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Self {
        Self {
            region: region.to_string(),
            swas: swas::SwasClient::new(access_key_id, access_key_secret, region),
            ecs: ecs::EcsClient::new(access_key_id, access_key_secret, region),
            cms: cms::CmsClient::new(access_key_id, access_key_secret, region),
        }
    }
}

pub struct AliyunProvider {
    groups: Vec<RegionGroup>,
}

impl AliyunProvider {
    /// 构造 provider。`region` 为空或 `global` 时自动发现账号下全部地域
    /// （SWAS ListRegions ∪ ECS DescribeRegions，去重保序），否则固定单地域。
    pub async fn new(access_key_id: &str, access_key_secret: &str, region: &str) -> Result<Self> {
        if is_global_region(region) {
            let regions = discover_regions(access_key_id, access_key_secret).await?;
            if regions.is_empty() {
                tracing::info!("region=global：目录未发现任何实例资源（无实例可巡检）");
            } else {
                tracing::info!(
                    "region=global：发现资源地域 {} 个: {}",
                    regions.len(),
                    regions.join(", ")
                );
            }
            Ok(Self {
                groups: regions
                    .into_iter()
                    .map(|r| RegionGroup::new(access_key_id, access_key_secret, &r))
                    .collect(),
            })
        } else {
            Ok(Self {
                groups: vec![RegionGroup::new(access_key_id, access_key_secret, region)],
            })
        }
    }

    /// 全部地域客户端组：global 模式 >1 个，单地域 = 1 个。
    /// 跨地域巡检（磁盘 / 自动快照 / ECS 到期）按组遍历汇总。
    pub fn groups(&self) -> &[RegionGroup] {
        &self.groups
    }

    /// 定位实例所属地域组：global 模式逐个探测 ListSnapshots
    /// （非所属地域对陌生实例 ID 返回 NotFound 类错误），成功即归属；
    /// 单地域配置直接返回唯一组（零额外调用）。
    async fn group_for_server(&self, server_id: &str) -> Result<&RegionGroup> {
        if self.groups.len() == 1 {
            return Ok(&self.groups[0]);
        }
        let mut last_err: Option<anyhow::Error> = None;
        for g in &self.groups {
            match g.swas.list_snapshots(server_id).await {
                Ok(_) => return Ok(g),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("实例 {server_id} 在任何地域均未找到")))
    }
}

/// region 为空或 "global"（不区分大小写）→ 自动发现全部地域
fn is_global_region(region: &str) -> bool {
    region.trim().is_empty() || region.eq_ignore_ascii_case(REGION_GLOBAL)
}

/// 自动发现账号下需要巡检的地域。
///
/// **目录优先**：资源中心 SearchResources 返回账号下**实际有实例的地域**（ECS ∪ SWAS），
/// 只巡检这些地域，避免遍历全部地域（32 个 × 每地域一次查询，含大量无权限/未开通地域噪音）。
/// 资源中心无权限/失败时回退到 ListRegions / DescribeRegions 全量列表（旧行为）。
async fn discover_regions(access_key_id: &str, access_key_secret: &str) -> Result<Vec<String>> {
    let rc = resourcecenter::ResourceCenterClient::new(access_key_id, access_key_secret);
    let swas = swas::SwasClient::new(access_key_id, access_key_secret, DISCOVERY_REGION);
    let ecs = ecs::EcsClient::new(access_key_id, access_key_secret, DISCOVERY_REGION);

    let mut regions: Vec<String> = Vec::new();
    let mut catalog_ok = false;

    // SWAS 目录：有实例的地域；失败回退 ListRegions（全地域）
    match rc.search_resources(resourcecenter::TYPE_SWAS_INSTANCE).await {
        Ok(list) => {
            catalog_ok = true;
            push_unique(&mut regions, list.into_iter().map(|r| r.region_id).collect());
        }
        Err(e) => {
            tracing::warn!("资源中心 SWAS 目录失败，回退 ListRegions（将遍历全部地域）: {e:#}");
            match swas.list_regions().await {
                Ok(list) => push_unique(&mut regions, list),
                Err(e2) => tracing::warn!("SWAS ListRegions 失败（global 将跳过轻量服务器）: {e2:#}"),
            }
        }
    }

    // ECS 目录：同理
    match rc.search_resources(resourcecenter::TYPE_ECS_INSTANCE).await {
        Ok(list) => {
            catalog_ok = true;
            push_unique(&mut regions, list.into_iter().map(|r| r.region_id).collect());
        }
        Err(e) => {
            tracing::warn!("资源中心 ECS 目录失败，回退 DescribeRegions（将遍历全部地域）: {e:#}");
            match ecs.describe_regions().await {
                Ok(list) => push_unique(&mut regions, list),
                Err(e2) => tracing::warn!("ECS DescribeRegions 失败（global 将跳过 ECS）: {e2:#}"),
            }
        }
    }

    // 目录成功但为空 = 账号确实无实例资源（正常，不报错）；
    // 全部发现方式失败且无任何地域才报错
    if regions.is_empty() && !catalog_ok {
        return Err(anyhow!(
            "自动发现地域失败：资源中心与 ListRegions/DescribeRegions 均未返回地域，\
             请检查 RAM 权限（resourcecenter:SearchResources）"
        ));
    }
    Ok(regions)
}

fn push_unique(regions: &mut Vec<String>, list: Vec<String>) {
    for r in list {
        if !regions.contains(&r) {
            regions.push(r);
        }
    }
}

/// 判断是否阿里云权限类错误（NoPermission / Forbidden 系列）。
/// global 模式下，账号未授予某产品权限（如纯 ECS 账号没有 SWAS 权限）
/// 应跳过该产品而不是报错，与"地域未开通"（Forbidden.Region 等）一并视为可跳过。
pub fn is_permission_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("NoPermission") || s.contains("Forbidden")
}

fn map_status(s: &str) -> SnapshotStatus {
    match s {
        // 阿里云轻量实际状态值：progressing（创建中）/ accomplished（已完成）
        "Creating" | "progressing" => SnapshotStatus::Creating,
        "accomplished" | "Available" => SnapshotStatus::Available,
        "Failed" => SnapshotStatus::Failed,
        _ => SnapshotStatus::Unknown,
    }
}

#[async_trait::async_trait]
impl CloudProvider for AliyunProvider {
    async fn list_servers(&self) -> Result<Vec<Server>> {
        let mut out = Vec::new();
        // 跨地域容忍：
        // - 权限类错误（未开通/未授权）→ 跳过该地域，不报错（如纯 ECS 账号无 SWAS 权限）
        // - 其他错误（网络/DNS/限流）→ 汇总记录，全部失败才报错
        let mut failed: Vec<String> = Vec::new();
        let mut no_perm: Vec<String> = Vec::new();
        let mut first_err: Option<anyhow::Error> = None;
        for g in &self.groups {
            match g.swas.list_instances().await {
                Ok(instances) => {
                    out.extend(instances.into_iter().map(|i| {
                        let id = i.instance_id.clone();
                        Server {
                            id,
                            name: if i.instance_name.is_empty() {
                                i.instance_id
                            } else {
                                i.instance_name
                            },
                            region: g.region.clone(),
                            status: i.status,
                            expired_at: parse_expired_time(&i.expired_time),
                        }
                    }));
                }
                Err(e) => {
                    if is_permission_error(&e) {
                        no_perm.push(g.region.clone());
                    } else {
                        failed.push(g.region.clone());
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
        }
        if !no_perm.is_empty() {
            tracing::warn!(
                "{} 个地域无 SWAS 权限（可能未购买/未授权），已跳过: {}",
                no_perm.len(),
                no_perm.join(", ")
            );
        }
        if !failed.is_empty() {
            // 该产品整体未授权/未购买（no_perm 非空）且无任何数据时，DNS/网络等失败
            // 属于"未开通地域"噪音，不应升级为致命错误
            if out.is_empty() && no_perm.is_empty() {
                return Err(anyhow!(
                    "全部 {} 个地域 SWAS 查询失败（首个错误: {:#}）",
                    failed.len(),
                    first_err.as_ref().map(|e| format!("{e:#}")).unwrap_or_default()
                ));
            }
            tracing::warn!(
                "{} 个地域 SWAS 查询失败，已跳过: {}",
                failed.len(),
                failed.join(", ")
            );
        }
        Ok(out)
    }

    async fn list_snapshots(&self, server_id: &str) -> Result<Vec<Snapshot>> {
        let g = self.group_for_server(server_id).await?;
        let snaps = g.swas.list_snapshots(server_id).await?;
        let mut out: Vec<Snapshot> = snaps
            .into_iter()
            .map(|s| {
                let id = s.snapshot_id.clone();
                Snapshot {
                    id,
                    name: if s.snapshot_name.is_empty() {
                        s.snapshot_id
                    } else {
                        s.snapshot_name
                    },
                    status: map_status(&s.status),
                    created_at: if s.creation_time.is_empty() {
                        None
                    } else {
                        Some(s.creation_time)
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    async fn create_snapshot(&self, server_id: &str, name: &str) -> Result<String> {
        let g = self.group_for_server(server_id).await?;
        g.swas.create_snapshot(server_id, name).await
    }

    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        if self.groups.len() == 1 {
            return self.groups[0].swas.delete_snapshot(snapshot_id).await;
        }
        // global：快照 ID 全局唯一，逐个地域尝试，成功即删除（非所属地域返回 NotFound）
        let mut last_err: Option<anyhow::Error> = None;
        for g in &self.groups {
            match g.swas.delete_snapshot(snapshot_id).await {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("快照 {snapshot_id} 在任何地域均未找到")))
    }

    async fn wait_snapshot_ready(
        &self,
        server_id: &str,
        snapshot_id: &str,
        timeout: Duration,
    ) -> Result<()> {
        // 先定位地域组（一次探测），再在本组内轮询，避免每次轮询都跨地域探测
        let g = self.group_for_server(server_id).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "等待快照 {snapshot_id} 就绪超时（{} 分钟）",
                    timeout.as_secs() / 60
                ));
            }
            let snaps = g.swas.list_snapshots(server_id).await?;
            match snaps.iter().find(|s| s.snapshot_id == snapshot_id) {
                Some(s) => {
                    let cur = format!("{} ({})", s.status, s.progress);
                    match map_status(&s.status) {
                        SnapshotStatus::Available => {
                            tracing::info!("快照 {snapshot_id} 已就绪");
                            return Ok(());
                        }
                        SnapshotStatus::Failed => {
                            return Err(anyhow!("快照 {snapshot_id} 创建失败（Failed）"));
                        }
                        _ => tracing::info!("等待快照 {snapshot_id}... 当前: {cur}"),
                    }
                }
                None => {
                    tracing::info!("等待快照 {snapshot_id}... 未在列表中");
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_global_region() {
        assert!(is_global_region(""));
        assert!(is_global_region("  "));
        assert!(is_global_region("global"));
        assert!(is_global_region("GLOBAL"));
        assert!(!is_global_region("cn-shenzhen"));
        assert!(!is_global_region("cn-hangzhou"));
    }

    #[test]
    fn test_is_permission_error() {
        // SWAS 403 NoPermission / ECS 403 Forbidden.RAM：权限类 → 跳过
        assert!(is_permission_error(&anyhow::anyhow!("阿里云 swas API HTTP 403 (ListInstances): NoPermission: User is not authorized.")));
        assert!(is_permission_error(&anyhow::anyhow!("阿里云 ecs API HTTP 403 (DescribeInstances): Forbidden.RAM: User not authorized.")));
        assert!(is_permission_error(&anyhow::anyhow!("阿里云 ecs API HTTP 403 (DescribeInstances): Forbidden.Region: region not enabled.")));
        // 网络/DNS/业务错误：非权限类 → 计入失败
        assert!(!is_permission_error(&anyhow::anyhow!("client error (Connect): dns error")));
        assert!(!is_permission_error(&anyhow::anyhow!("阿里云 swas 业务错误 Throttling: Request was denied")));
        assert!(!is_permission_error(&anyhow::anyhow!("响应解析失败 (ListInstances)")));
    }
}
