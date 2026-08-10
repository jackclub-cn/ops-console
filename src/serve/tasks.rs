//! 任务队列与执行（任务 4 整体替换为真实实现，此处为编译用桩）。
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

#[derive(Debug, Clone, Deserialize)]
pub struct CommandSpec {
    pub command: String,
    pub project: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    pub id: String,
    pub submitted_at: String,
    pub command: String,
    pub args: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_secs: Option<u64>,
    pub output_file: String,
}

#[derive(Debug, Clone)]
pub struct TaskManager;

impl TaskManager {
    pub fn new(_dir: &Path) -> anyhow::Result<Arc<Self>> { Ok(Arc::new(Self)) }
    pub fn submit(&self, _spec: CommandSpec) -> anyhow::Result<TaskMeta> { anyhow::bail!("任务模块未实现") }
    pub fn list(&self) -> Vec<TaskMeta> { Vec::new() }
    pub fn current(&self) -> Option<TaskMeta> { None }
    pub fn read_output(&self, _id: &str) -> Option<String> { None }
    pub async fn sse_current(&self) -> axum::response::Response {
        axum::http::StatusCode::NOT_IMPLEMENTED.into_response()
    }
}
