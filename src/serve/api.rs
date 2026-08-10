//! 配置管理 API：登录、结构化配置读写、YAML 原文读写、任务提交与查询。

use super::{AppState, tasks};
use crate::config::{self, Config};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::path::Path;

/// secret 掩码标记：表单提交时等于该值 = 未修改，保留原值。
pub const SECRET_MASK: &str = "••••••••";

pub async fn login(State(state): State<AppState>, Json(body): Json<serde_json::Value>) -> Response {
    let token = body["token"].as_str().unwrap_or("");
    if state.validator.verify_query(token) {
        (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// 读取文件中的结构化配置（不做环境变量覆盖）。
fn read_config_files(dir: &Path) -> anyhow::Result<(Vec<config::Project>, config::NotifyConfig)> {
    let project_text = std::fs::read_to_string(dir.join("project.yml"))
        .map_err(|e| anyhow::anyhow!("读取 project.yml 失败: {e}"))?;
    let notify_text = std::fs::read_to_string(dir.join("notify.yml")).ok();
    let cfg = Config::from_str(&project_text, notify_text.as_deref())?;
    Ok((cfg.projects, cfg.notify))
}

fn mask_secret(v: &Option<String>) -> Option<String> {
    v.as_ref().filter(|s| !s.is_empty()).map(|_| SECRET_MASK.to_string())
}

fn unmask_secret(submitted: &Option<String>, original: &Option<String>) -> Option<String> {
    match submitted {
        Some(s) if s == SECRET_MASK => original.clone(),
        other => other.clone(),
    }
}

pub async fn get_config(State(state): State<AppState>) -> Response {
    match read_config_files(&state.config_dir) {
        Ok((projects, notify)) => {
            let mut projects = projects;
            for p in &mut projects {
                for pc in p.providers.values_mut() {
                    pc.access_key_secret = mask_secret(&pc.access_key_secret);
                }
            }
            let mut notify = notify;
            if !notify.dingtalk.secret.is_empty() {
                notify.dingtalk.secret = SECRET_MASK.to_string();
            }
            (StatusCode::OK, Json(serde_json::json!({"projects": projects, "notify": notify})))
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ConfigPayload {
    projects: Vec<config::Project>,
    notify: config::NotifyConfig,
}

pub async fn save_config(State(state): State<AppState>, Json(payload): Json<ConfigPayload>) -> Response {
    // 掩码还原：读当前文件原值
    let (orig_projects, orig_notify) = match read_config_files(&state.config_dir) {
        Ok(v) => v,
        Err(_) => (Vec::new(), config::NotifyConfig::default()),
    };
    let mut projects = payload.projects;
    for p in &mut projects {
        if let Some(orig) = orig_projects.iter().find(|o| o.name == p.name) {
            for (kind, pc) in &mut p.providers {
                if let Some(opc) = orig.providers.get(kind) {
                    pc.access_key_secret = unmask_secret(&pc.access_key_secret, &opc.access_key_secret);
                }
            }
        }
    }
    let mut notify = payload.notify;
    if notify.dingtalk.secret == SECRET_MASK {
        notify.dingtalk.secret = orig_notify.dingtalk.secret.clone();
    }

    // 序列化 + 校验
    let project_yaml = match serde_yaml::to_string(&projects) {
        Ok(y) => y,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("序列化失败: {e}")).into_response(),
    };
    let notify_yaml = serde_yaml::to_string(&notify).unwrap_or_default();
    if let Err(e) = Config::from_str(&project_yaml, Some(&notify_yaml)) {
        return (StatusCode::BAD_REQUEST, format!("校验失败: {e:#}")).into_response();
    }

    // 写盘
    match config::write_atomic(&state.config_dir.join("project.yml"), &project_yaml)
        .and_then(|_| config::write_atomic(&state.config_dir.join("notify.yml"), &notify_yaml))
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("写盘失败: {e:#}")).into_response(),
    }
}

pub async fn get_raw(State(state): State<AppState>) -> Response {
    let project_yml = std::fs::read_to_string(state.config_dir.join("project.yml"))
        .unwrap_or_default();
    let notify_yml = std::fs::read_to_string(state.config_dir.join("notify.yml")).ok();
    (StatusCode::OK, Json(serde_json::json!({"project_yml": project_yml, "notify_yml": notify_yml})))
        .into_response()
}

#[derive(Deserialize)]
pub struct RawPayload {
    project_yml: String,
    notify_yml: Option<String>,
}

pub async fn save_raw(State(state): State<AppState>, Json(payload): Json<RawPayload>) -> Response {
    // 空/缺省 notify.yml = 不通知；校验与写盘均按此语义处理
    let notify_text = payload.notify_yml.unwrap_or_default();
    let notify_ref = (!notify_text.trim().is_empty()).then_some(notify_text.as_str());
    if let Err(e) = Config::from_str(&payload.project_yml, notify_ref) {
        return (StatusCode::BAD_REQUEST, format!("校验失败: {e:#}")).into_response();
    }
    let r = config::write_atomic(&state.config_dir.join("project.yml"), &payload.project_yml)
        .and_then(|_| config::write_atomic(&state.config_dir.join("notify.yml"), &notify_text));
    match r {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("写盘失败: {e:#}")).into_response(),
    }
}

pub async fn run(State(state): State<AppState>, Json(spec): Json<tasks::CommandSpec>) -> Response {
    match state.tasks.submit(spec) {
        Ok(meta) => (StatusCode::OK, Json(serde_json::json!({"task_id": meta.id}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("提交失败: {e:#}")).into_response(),
    }
}

pub async fn list_tasks(State(state): State<AppState>) -> Response {
    let tasks = state.tasks.list();
    (StatusCode::OK, Json(serde_json::json!(tasks))).into_response()
}

pub async fn current_task(State(state): State<AppState>) -> Response {
    let cur = state.tasks.current();
    (StatusCode::OK, Json(serde_json::json!(cur))).into_response()
}

pub async fn stream_current(State(state): State<AppState>) -> Response {
    state.tasks.sse_current().await
}

pub async fn task_output(State(state): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    match state.tasks.read_output(&id) {
        Some(text) => (StatusCode::OK, text).into_response(),
        None => (StatusCode::NOT_FOUND, "输出文件缺失").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::{auth, require_auth};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project.yml"),
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n      access_key_secret: real-secret\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("notify.yml"),
            "kind: dingtalk\nprefix: \"【测试】\"\ndingtalk:\n  webhook: https://example.com/hook\n",
        )
        .unwrap();
        let tasks = tasks::TaskManager::new(&dir).unwrap(); // 已返回 Arc<Self>
        AppState {
            config_dir: dir,
            validator: auth::TokenValidator::new("tok"),
            tasks,
        }
    }

    fn app(state: AppState) -> axum::Router {
        Router::new()
            .route("/api/login", axum::routing::post(login))
            .route("/api/config", axum::routing::get(get_config).post(save_config))
            .route("/api/config/raw", axum::routing::get(get_raw).post(save_raw))
            .route("/api/tasks", axum::routing::get(list_tasks))
            .with_state(state.clone())
            .route_layer(middleware::from_fn_with_state(state, require_auth))
    }

    fn auth_req(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        b = b.header("authorization", "Bearer tok");
        match body {
            Some(t) => b
                .header("content-type", "application/json")
                .body(Body::from(t.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        }
    }

    #[tokio::test]
    async fn test_login_ok_and_bad() {
        let st = test_state();
        let app = app(st.clone());
        let ok = app.clone().oneshot(
            Request::builder().method("POST").uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"tok"}"#)).unwrap(),
        ).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let bad = app.oneshot(
            Request::builder().method("POST").uri("/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"wrong"}"#)).unwrap(),
        ).await.unwrap();
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_config_get_masks_secret() {
        let st = test_state();
        let app = app(st.clone());
        let res = app.oneshot(auth_req("GET", "/api/config", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let secret = v["projects"][0]["providers"]["aliyun"]["access_key_secret"].as_str().unwrap();
        assert_eq!(secret, SECRET_MASK, "secret 应被掩码");
        assert_eq!(v["notify"]["prefix"].as_str().unwrap(), "【测试】");
    }

    #[tokio::test]
    async fn test_config_post_mask_not_overwritten() {
        let st = test_state();
        let app = app(st.clone());
        // 提交掩码值 → 服务端保留文件原值
        let body = r#"{"projects":[{"name":"demo","description":null,"providers":{"aliyun":{"region":"cn-shenzhen","access_key_id":null,"access_key_secret":"••••••••"}}}],"notify":{"kind":"dingtalk","prefix":"【测试】","dingtalk":{"webhook":"https://example.com/hook","secret":""}}}"#;
        let res = app.oneshot(auth_req("POST", "/api/config", Some(body))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(file.contains("real-secret"), "掩码不应覆盖真实 secret: {file}");
    }

    #[tokio::test]
    async fn test_config_post_invalid_rejected() {
        let st = test_state();
        let app = app(st.clone());
        let body = r#"{"projects":[],"notify":{"kind":"none"}}"#;
        let res = app.oneshot(auth_req("POST", "/api/config", Some(body))).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // 原文件未被改动
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(file.contains("demo"));
    }

    #[tokio::test]
    async fn test_raw_roundtrip() {
        let st = test_state();
        let app = app(st.clone());
        let res = app.clone().oneshot(auth_req("GET", "/api/config/raw", None)).await.unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["project_yml"].as_str().unwrap().contains("demo"));

        let new_yaml = "- name: prod\n  providers:\n    aliyun:\n      region: cn-hangzhou\n";
        let payload = format!(r#"{{"project_yml": {new_yaml:?}, "notify_yml": null}}"#);
        let res = app.clone().oneshot(auth_req("POST", "/api/config/raw", Some(&payload))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(file.contains("prod"));
        assert!(!file.contains("demo"));
    }

    #[tokio::test]
    async fn test_401_without_token() {
        let st = test_state();
        let app = app(st.clone());
        let res = app.oneshot(
            Request::builder().method("GET").uri("/api/config")
                .body(Body::empty()).unwrap(),
        ).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
