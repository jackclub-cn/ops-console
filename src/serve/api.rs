//! 配置管理 API（任务 3 整体替换为真实实现，此处为编译用桩）。
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use super::AppState;

pub async fn login(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn get_config(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn save_config(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn get_raw(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn save_raw(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn run(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn list_tasks(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn current_task(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn stream_current(State(_): State<AppState>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
pub async fn task_output(State(_): State<AppState>, _p: axum::extract::Path<String>) -> Response { StatusCode::NOT_IMPLEMENTED.into_response() }
