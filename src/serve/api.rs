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

/// 在 orig 中定位提交项目的原项目：优先 _path（唯一稳定索引，改名后仍命中）；
/// _path 缺省/为 null（异常）时 fallback 到按 name 匹配（保持旧行为）。
/// _path 存在但无法匹配 → 视为新项目（返回 None，不还原掩码）。
fn find_orig_project<'a>(orig: &'a [config::Project], path: Option<&str>, name: &str) -> Option<&'a config::Project> {
    match path {
        Some(p) => p.parse::<usize>().ok().and_then(|i| orig.get(i)),
        None => orig.iter().find(|o| o.name == name),
    }
}

/// 定位提交服务商的原服务商：优先 provider._path（"<项目_path>/<kind>"），
/// 其次项目 _path + kind，最后 name + kind。定位不到返回 None（不还原）。
fn find_orig_provider<'a>(
    orig: &'a [config::Project],
    proj_path: Option<&str>,
    proj_name: &str,
    kind: &str,
    prov_path: Option<&str>,
) -> Option<&'a config::ProviderConfig> {
    if let Some(pp) = prov_path {
        // provider._path 存在：拆出项目段与 kind 段，两者都匹配才还原
        let (p, k) = pp.rsplit_once('/')?;
        if k != kind {
            return None;
        }
        return find_orig_project(orig, Some(p), proj_name).and_then(|op| op.providers.get(kind));
    }
    find_orig_project(orig, proj_path, proj_name).and_then(|op| op.providers.get(kind))
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
            // 附加稳定内部索引 _path：项目为下标（"0"、"1"...），服务商为 "<项目_path>/<kind>"，
            // 供前端原样回传、服务端按索引还原掩码（改名/删建后仍能定位原值）。
            let mut body = serde_json::json!({ "projects": projects, "notify": notify });
            if let Some(arr) = body["projects"].as_array_mut() {
                for (i, pv) in arr.iter_mut().enumerate() {
                    if let Some(obj) = pv.as_object_mut() {
                        obj.insert("_path".into(), serde_json::json!(i.to_string()));
                        if let Some(providers) = obj.get_mut("providers").and_then(|v| v.as_object_mut()) {
                            for (kind, pc) in providers.iter_mut() {
                                if let Some(pc_obj) = pc.as_object_mut() {
                                    pc_obj.insert("_path".into(), serde_json::json!(format!("{i}/{kind}")));
                                }
                            }
                        }
                    }
                }
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ConfigPayload {
    /// 用 Value 承接以保留各项目/服务商的 _path 字段（config::Project 不感知该字段）。
    projects: Vec<serde_json::Value>,
    notify: config::NotifyConfig,
}

pub async fn save_config(State(state): State<AppState>, Json(payload): Json<ConfigPayload>) -> Response {
    // 掩码还原：读当前文件原值
    let (orig_projects, orig_notify) = match read_config_files(&state.config_dir) {
        Ok(v) => v,
        Err(_) => (Vec::new(), config::NotifyConfig::default()),
    };
    let mut projects: Vec<config::Project> = Vec::with_capacity(payload.projects.len());
    for pv in payload.projects {
        let proj_path = pv.get("_path").and_then(|v| v.as_str()).map(str::to_string);
        let mut p: config::Project = match serde_json::from_value(pv.clone()) {
            Ok(p) => p,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("解析项目配置失败: {e}")).into_response(),
        };
        for (kind, pc) in &mut p.providers {
            let prov_path = pv["providers"]
                .get(kind)
                .and_then(|v| v.get("_path"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match find_orig_provider(&orig_projects, proj_path.as_deref(), &p.name, kind, prov_path.as_deref()) {
                Some(opc) => pc.access_key_secret = unmask_secret(&pc.access_key_secret, &opc.access_key_secret),
                None => {
                    // 定位不到原值（新增项目/新增服务商）：不还原，直接存提交值；
                    // 但提交值恰为掩码属异常，保守置空，绝不把掩码字面量写进文件。
                    if pc.access_key_secret.as_deref() == Some(SECRET_MASK) {
                        pc.access_key_secret = None;
                    }
                }
            }
        }
        projects.push(p);
    }
    let mut notify = payload.notify;
    if notify.dingtalk.secret == SECRET_MASK {
        notify.dingtalk.secret = orig_notify.dingtalk.secret.clone();
    }

    // 项目名唯一性校验（表单模式：前端可新增同名项目，保存后会导致目标歧义）
    {
        let mut seen = std::collections::HashSet::new();
        for p in &projects {
            if !seen.insert(p.name.clone()) {
                return (StatusCode::BAD_REQUEST, format!("项目名重复: {}", p.name)).into_response();
            }
        }
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
    let notify_path = state.config_dir.join("notify.yml");
    let write_project = config::write_atomic(&state.config_dir.join("project.yml"), &payload.project_yml);
    let write_notify = if notify_text.trim().is_empty() {
        // 空/缺省 notify.yml = 不通知：删除 notify.yml（若存在），不落空文件
        match std::fs::remove_file(&notify_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::anyhow!("删除 {} 失败: {e}", notify_path.display())),
        }
    } else {
        config::write_atomic(&notify_path, &notify_text)
    };
    match write_project.and_then(|_| write_notify) {
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
            .route("/api/tasks/current/stream", axum::routing::get(stream_current))
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
    async fn test_stream_requires_auth() {
        let st = test_state();
        let app = app(st.clone());
        // 无 token → 401
        let res = app
            .clone()
            .oneshot(Request::builder().method("GET").uri("/api/tasks/current/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        // 带 token → 200
        let ok = app
            .oneshot(auth_req("GET", "/api/tasks/current/stream", None))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_config_duplicate_project_name_rejected() {
        let st = test_state();
        let app = app(st.clone());
        let body = r#"{"projects":[{"name":"demo","providers":{}},{"name":"demo","providers":{}}],"notify":{}}"#;
        let res = app.oneshot(auth_req("POST", "/api/config", Some(body))).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "重复项目名应被 400 拒绝");
    }

    #[tokio::test]
    async fn test_save_raw_empty_notify_removes_file() {
        let st = test_state();
        // 预置 notify.yml 存在
        assert!(st.config_dir.join("notify.yml").exists());
        let app = app(st.clone());
        let body = r#"{"project_yml":"- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n","notify_yml":""}"#;
        let res = app.oneshot(auth_req("POST", "/api/config/raw", Some(body))).await.unwrap();
        let status = res.status();
        if status != StatusCode::OK {
            let body = res.into_body().collect().await.unwrap().to_bytes();
            panic!("expected 200 got {status}: {}", String::from_utf8_lossy(&body));
        }
        assert!(!st.config_dir.join("notify.yml").exists(), "空 notify_yml 应删除 notify.yml 而非写空文件");
        // project.yml 仍在
        assert!(st.config_dir.join("project.yml").exists());
    }

    #[tokio::test]
    async fn test_config_get_adds_path_fields() {
        let st = test_state();
        let app = app(st.clone());
        let res = app.oneshot(auth_req("GET", "/api/config", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["projects"][0]["_path"].as_str(), Some("0"), "GET 应附带项目 _path");
        assert_eq!(
            v["projects"][0]["providers"]["aliyun"]["_path"].as_str(),
            Some("0/aliyun"),
            "GET 应附带服务商 _path"
        );
    }

    #[tokio::test]
    async fn test_config_path_roundtrip_keeps_secret() {
        let st = test_state();
        let app = app(st.clone());
        // GET 原样回传（含 _path 与掩码）→ 文件保留真实 secret
        let res = app.clone().oneshot(auth_req("GET", "/api/config", None)).await.unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let res = app.oneshot(auth_req("POST", "/api/config", Some(&v.to_string()))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(file.contains("real-secret"), "_path 往返后掩码不应覆盖真实 secret: {file}");
        assert!(!file.contains(SECRET_MASK), "掩码字面量绝不应写入文件: {file}");
    }

    #[tokio::test]
    async fn test_config_rename_with_path_keeps_secret() {
        let st = test_state();
        let app = app(st.clone());
        let res = app.clone().oneshot(auth_req("GET", "/api/config", None)).await.unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // 改名但保留 _path → 掩码还原仍应命中原项目
        v["projects"][0]["name"] = serde_json::json!("renamed");
        let res = app.oneshot(auth_req("POST", "/api/config", Some(&v.to_string()))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(file.contains("renamed"), "改名应生效: {file}");
        assert!(file.contains("real-secret"), "改名后掩码不应覆盖真实 secret: {file}");
        assert!(!file.contains(SECRET_MASK), "掩码字面量绝不应写入文件: {file}");
    }

    #[tokio::test]
    async fn test_config_new_provider_empty_secret_stored() {
        let st = test_state();
        let app = app(st.clone());
        // 新增服务商（无 _path）+ 空 secret → 直接存提交值；已有 aliyun 掩码按 name fallback 还原
        let body = r#"{"projects":[{"name":"demo","description":null,"providers":{"aliyun":{"region":"cn-shenzhen","access_key_id":null,"access_key_secret":"••••••••"},"tencent":{"region":"ap-guangzhou","access_key_id":null,"access_key_secret":""}}}],"notify":{"kind":"dingtalk","prefix":"【测试】","dingtalk":{"webhook":"https://example.com/hook","secret":""}}}"#;
        let res = app.oneshot(auth_req("POST", "/api/config", Some(body))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(file.contains("tencent"), "新增服务商应写入: {file}");
        assert!(file.contains("real-secret"), "aliyun 真实 secret 应保留: {file}");
        assert!(!file.contains(SECRET_MASK), "掩码字面量绝不应写入文件: {file}");
    }

    #[tokio::test]
    async fn test_config_stray_mask_not_written() {
        let st = test_state();
        let app = app(st.clone());
        // 异常情况：_path 定位不到原值但提交了掩码 → 保守处理，绝不写掩码字面量
        let body = r#"{"projects":[{"name":"ghost","description":null,"_path":"9","providers":{"aliyun":{"region":"cn-shenzhen","access_key_id":null,"access_key_secret":"••••••••"}}}],"notify":{"kind":"none"}}"#;
        let res = app.oneshot(auth_req("POST", "/api/config", Some(body))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let file = std::fs::read_to_string(st.config_dir.join("project.yml")).unwrap();
        assert!(!file.contains(SECRET_MASK), "掩码字面量绝不应写入文件: {file}");
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
