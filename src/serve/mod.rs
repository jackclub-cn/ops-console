//! Web 管理界面：axum HTTP 服务 + 任务队列 + 配置管理 API。

pub mod api;
pub mod auth;
pub mod tasks;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{path::PathBuf, sync::Arc};

/// 全局共享状态。
#[derive(Clone)]
pub struct AppState {
    pub config_dir: PathBuf,
    pub validator: auth::TokenValidator,
    pub tasks: Arc<tasks::TaskManager>,
}

const INDEX_HTML: &str = include_str!("static/index.html");
const APP_JS: &str = include_str!("static/app.js");
const BOOTSTRAP_CSS: &str = include_str!("static/vendor/bootstrap.min.css");
const BOOTSTRAP_JS: &str = include_str!("static/vendor/bootstrap.bundle.min.js");

/// 启动 Web 服务（serve 子命令入口）。
pub async fn run(
    config_dir: &PathBuf,
    addr: &str,
    token_override: Option<String>,
) -> anyhow::Result<()> {
    // 配置目录不存在时自动创建（UI 可引导首次配置，是 serve 的核心功能）
    std::fs::create_dir_all(config_dir)
        .map_err(|e| anyhow::anyhow!("创建配置目录失败 {}: {e}", config_dir.display()))?;
    let (token, generated) = auth::resolve_token(config_dir, token_override, |k| std::env::var(k).ok())?;
    let state = AppState {
        config_dir: config_dir.clone(),
        validator: auth::TokenValidator::new(&token),
        tasks: tasks::TaskManager::new(config_dir)?, // 返回 Arc<Self>（内部已启动 worker）
    };

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/static/{file}", get(serve_static))
        .route("/api/login", axum::routing::post(api::login))
        .route(
            "/api/config",
            axum::routing::get(api::get_config).post(api::save_config),
        )
        .route(
            "/api/config/raw",
            axum::routing::get(api::get_raw).post(api::save_raw),
        )
        .route("/api/run", axum::routing::post(api::run))
        .route("/api/tasks", axum::routing::get(api::list_tasks))
        .route("/api/tasks/current", axum::routing::get(api::current_task))
        .route(
            "/api/tasks/current/stream",
            axum::routing::get(api::stream_current),
        )
        .route("/api/tasks/{id}/output", axum::routing::get(api::task_output))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("Web UI 已启动: http://{addr}");
    // 仅首次自动生成时打印令牌（避免每次启动把既有令牌输出到 stdout/日志）
    if generated {
        println!("已生成新的访问令牌: {token}（仅此一次显示，请妥善保存）");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// 静态页面。
async fn serve_index() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(INDEX_HTML))
        .unwrap()
}

/// 静态资源（bootstrap / app.js）。
async fn serve_static(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    let (body, mime) = match file.as_str() {
        "app.js" => (APP_JS, "application/javascript; charset=utf-8"),
        "bootstrap.min.css" => (BOOTSTRAP_CSS, "text/css; charset=utf-8"),
        "bootstrap.bundle.min.js" => (BOOTSTRAP_JS, "application/javascript; charset=utf-8"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from(body))
        .unwrap()
}

/// 认证中间件：静态资源与 /api/login 放行，其余校验 Bearer 或 ?token=。
async fn require_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if path == "/" || path.starts_with("/static/") || path == "/api/login" {
        return next.run(req).await;
    }
    let ok = state
        .validator
        .verify_header(
            req.headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        )
        || (path == "/api/tasks/current/stream"
            && state
                .validator
                .verify_query(
                    req.uri()
                        .query()
                        .and_then(|q| {
                            q.split('&')
                                .find_map(|kv| kv.strip_prefix("token="))
                        })
                        .unwrap_or(""),
                ));
    if ok {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.yml"), "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n").unwrap();
        let tasks = tasks::TaskManager::new(&dir).unwrap(); // 已返回 Arc<Self>
        AppState {
            config_dir: dir,
            validator: auth::TokenValidator::new("tok"),
            tasks,
        }
    }

    #[tokio::test]
    async fn test_static_and_auth_routes() {
        let state = test_state();
        let dir = state.config_dir.clone();
        let app = Router::new()
            .route("/", get(serve_index))
            .route("/static/{file}", get(serve_static))
            .route("/api/tasks", axum::routing::get(api::list_tasks))
            .with_state(state.clone())
            .route_layer(middleware::from_fn_with_state(state, require_auth));

        let idx = app.clone().oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(idx.status(), StatusCode::OK);
        let body = idx.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("ops-console"));

        let css = app.clone().oneshot(Request::builder().uri("/static/bootstrap.min.css").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(css.status(), StatusCode::OK);

        let noauth = app.clone().oneshot(Request::builder().uri("/api/tasks").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(noauth.status(), StatusCode::UNAUTHORIZED);
        std::fs::remove_dir_all(dir).ok();
    }
}
