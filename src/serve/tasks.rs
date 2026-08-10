//! 任务队列与执行：单 worker 队列，spawn 自身二进制子进程，SSE 流式输出，历史持久化。

use axum::response::{IntoResponse, sse::{Event, KeepAlive, Sse}};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{broadcast, Notify},
};

/// 一次子命令执行请求（/api/run 的 body）。
#[derive(Debug, Clone, Deserialize)]
pub struct CommandSpec {
    pub command: String,
    pub project: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub extra: Vec<String>,
}

/// 任务元数据（jsonl 每行一条 + 内存列表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    pub id: String,
    pub submitted_at: String,
    pub command: String,
    pub args: Vec<String>,
    pub status: String, // queued | running | success | failed
    pub exit_code: Option<i32>,
    pub duration_secs: Option<u64>,
    pub output_file: String,
}

struct RunningTask {
    meta: TaskMeta,
}

struct TaskStore {
    queue: VecDeque<TaskMeta>,
    current: Option<RunningTask>,
    history: Vec<TaskMeta>,
}

/// 任务管理器：提交入队、单 worker 执行、SSE 广播、历史落盘。
pub struct TaskManager {
    config_dir: PathBuf,
    store: Mutex<TaskStore>,
    notify: Notify,
    tx: broadcast::Sender<String>,
}

impl TaskManager {
    /// 创建管理器并启动后台 worker（tokio 任务）。返回 Arc 便于 worker 与 AppState 共享。
    pub fn new(config_dir: &Path) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(config_dir.join("tasks"))?;
        let (tx, _rx) = broadcast::channel(1024);
        let history = load_history(config_dir);
        let store = Mutex::new(TaskStore { queue: VecDeque::new(), current: None, history });
        let mgr = Arc::new(Self {
            config_dir: config_dir.to_path_buf(),
            store,
            notify: Notify::new(),
            tx,
        });
        let worker = mgr.clone();
        tokio::spawn(async move {
            loop {
                worker.notify.notified().await;
                while worker.run_next_if_idle().await.unwrap_or(false) {}
            }
        });
        Ok(mgr)
    }

    /// 提交任务入队，返回任务元数据。
    pub fn submit(&self, spec: CommandSpec) -> anyhow::Result<TaskMeta> {
        let extra = validate_spec(&spec)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let meta = TaskMeta {
            id: id.clone(),
            submitted_at: chrono::Utc::now().to_rfc3339(),
            command: spec.command.clone(),
            args: build_args(
                &self.config_dir,
                spec.project.as_deref(),
                spec.provider.as_deref(),
                &spec.command,
                &extra,
            ),
            status: "queued".into(),
            exit_code: None,
            duration_secs: None,
            output_file: format!("tasks/{id}.log"),
        };
        let mut store = self.store.lock().expect("task store");
        store.queue.push_back(meta.clone());
        self.notify.notify_one();
        Ok(meta)
    }

    pub fn list(&self) -> Vec<TaskMeta> {
        let store = self.store.lock().expect("task store");
        store.history.clone()
    }

    pub fn current(&self) -> Option<TaskMeta> {
        let store = self.store.lock().expect("task store");
        store.current.as_ref().map(|t| t.meta.clone())
    }

    /// 从队列取一个任务执行；队列空返回 false（worker 循环与测试共用）。
    pub async fn run_next_if_idle(&self) -> anyhow::Result<bool> {
        let meta = {
            let mut store = self.store.lock().expect("task store");
            match store.queue.pop_front() {
                Some(m) => {
                    store.current = Some(RunningTask { meta: m.clone() });
                    m
                }
                None => return Ok(false),
            }
        };
        self.execute(&meta).await;
        Ok(true)
    }

    async fn execute(&self, meta: &TaskMeta) {
        // 标记 running
        {
            let mut store = self.store.lock().expect("task store");
            if let Some(cur) = &mut store.current {
                cur.meta.status = "running".into();
            }
        }
        self.emit(&meta.id, "__status__ running");
        let started = Instant::now();
        let log_path = self.config_dir.join(&meta.output_file);
        let mut log = match std::fs::File::create(&log_path) {
            Ok(f) => f,
            Err(e) => {
                let _ = self.finish(meta, Some(-1), started, &format!("无法创建输出文件: {e}")).await;
                return;
            }
        };

        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                let _ = self.finish(meta, Some(-1), started, &format!("无法定位自身可执行文件: {e}")).await;
                return;
            }
        };
        let mut child = match tokio::process::Command::new(&exe)
            .args(&meta.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = self.finish(meta, Some(-1), started, &format!("启动子进程失败: {e}")).await;
                return;
            }
        };

        // stdout/stderr 合并逐行读取：select 双流循环，直到两流都 EOF
        let stdout = child.stdout.take().expect("stdout 管道");
        let stderr = child.stderr.take().expect("stderr 管道");
        let mut out_lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                line = out_lines.next_line(), if !out_done => match line {
                    Ok(Some(l)) => self.push_line(&meta.id, &l, &mut log).await,
                    _ => out_done = true,
                },
                line = err_lines.next_line(), if !err_done => match line {
                    Ok(Some(l)) => self.push_line(&meta.id, &l, &mut log).await,
                    _ => err_done = true,
                },
            }
        }

        let code = match child.wait().await {
            Ok(st) => st.code(),
            Err(_) => Some(-1),
        };
        let _ = self.finish(meta, code, started, "").await;
    }

    async fn push_line(&self, task_id: &str, line: &str, log: &mut std::fs::File) {
        let _ = writeln!(log, "{line}");
        let _ = log.flush();
        self.emit(task_id, line);
    }

    /// 广播消息格式："{task_id}\t{content}"（tab 分隔），供 SSE 按任务归属过滤。
    fn emit(&self, task_id: &str, line: &str) {
        let _ = self.tx.send(format!("{task_id}\t{line}"));
    }

    /// 收尾：写历史、更新状态、广播完成事件、追加 jsonl。
    async fn finish(&self, meta: &TaskMeta, exit_code: Option<i32>, started: Instant, error: &str) -> anyhow::Result<()> {
        if !error.is_empty() {
            let log_path = self.config_dir.join(&meta.output_file);
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                let _ = writeln!(f, "{error}");
            }
            self.emit(&meta.id, error);
        }
        let status = if exit_code == Some(0) { "success" } else { "failed" };
        let mut m = meta.clone();
        m.status = status.into();
        m.exit_code = exit_code;
        m.duration_secs = Some(started.elapsed().as_secs());
        {
            let mut store = self.store.lock().expect("task store");
            store.current = None;
            store.history.push(m.clone());
            if let Err(e) = append_history(&self.config_dir, &m) {
                // 历史写入失败不中断收尾：告警落日志 + 作为普通行广播，保证终态事件必然发出
                tracing::warn!(task_id = %m.id, status, "追加历史到 jsonl 失败: {e:#}");
                let msg = format!("警告: 历史记录写入失败: {e:#}");
                let log_path = self.config_dir.join(&meta.output_file);
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                    let _ = writeln!(f, "{msg}");
                }
                self.emit(&meta.id, &msg);
            }
        }
        self.emit(&meta.id, &format!("__status__ {status}"));
        Ok(())
    }

    /// SSE：订阅任务输出流。事件格式：
    /// - `event: task`，data = 当前任务 id（无任务时为空串），声明后续 tail 重放行归属；
    /// - `event: line`，data = 输出行；
    /// - `event: status`，data = "{task_id} {status}"（前端按任务 id 过滤，排队场景不误判）。
    /// 流不因某个任务的终态而自行关闭：终态后如有排队任务继续广播，由前端收到匹配
    /// 自己任务的终态后主动 es.close() 断开。
    pub async fn sse_current(&self) -> axum::response::Response {
        let rx = self.tx.subscribe();
        let store = self.store.lock().expect("task store");
        let current_id = store
            .current
            .as_ref()
            .map(|t| t.meta.id.clone())
            .unwrap_or_default();
        let tail = match &store.current {
            Some(t) => {
                let p = self.config_dir.join(&t.meta.output_file);
                std::fs::read_to_string(&p).unwrap_or_default()
            }
            None => String::new(),
        };
        let current_status = store.current.as_ref().map(|t| t.meta.status.clone());
        drop(store);

        let stream = async_stream::stream! {
            // 先声明当前任务（无任务时 data 为空串），随后 tail 重放行属于该任务
            yield Ok::<_, std::convert::Infallible>(Event::default().event("task").data(current_id.clone()));
            // 历史尾部（最后 200 行）
            for line in tail.lines().rev().take(200).collect::<Vec<_>>().into_iter().rev() {
                yield Ok(Event::default().event("line").data(line.to_string()));
            }
            if let Some(st) = &current_status {
                yield Ok(Event::default().event("status").data(format!("{current_id} {st}")));
            }
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        let Some((task_id, kind, payload)) = parse_broadcast(&msg) else {
                            continue;
                        };
                        if kind == "status" {
                            yield Ok(Event::default().event("status").data(format!("{task_id} {payload}")));
                        } else {
                            yield Ok(Event::default().event("line").data(payload));
                        }
                    }
                    // 消费者追不上：丢行降级，继续读，不静默断流
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    // 所有发送端已关闭（TaskManager 被销毁）：正常结束
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
    }

    /// 读取历史任务完整输出。id 必须是 32 位 hex（任务 id 恒为 uuid simple 格式），
    /// 拒绝含路径分隔符 / .. 等任意非法 id，防路径穿越读取 tasks 目录外文件。
    pub fn read_output(&self, id: &str) -> Option<String> {
        if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read_to_string(self.config_dir.join("tasks").join(format!("{id}.log"))).ok()
    }
}

/// 解析广播消息 "{task_id}\t{content}" → (task_id, kind, payload)。
/// kind: "status"（content 以 "__status__ " 开头，payload 为状态名）| "line"（payload 为原行）。
/// 无 tab 或 task_id 为空 → None（容错：不中断 SSE 流）。
fn parse_broadcast(msg: &str) -> Option<(&str, &str, &str)> {
    let (task_id, content) = msg.split_once('\t')?;
    if task_id.is_empty() {
        return None;
    }
    if let Some(st) = content.strip_prefix("__status__ ") {
        Some((task_id, "status", st))
    } else {
        Some((task_id, "line", content))
    }
}

/// 校验命令并返回命令专属参数列表。
fn validate_spec(spec: &CommandSpec) -> anyhow::Result<Vec<String>> {
    let mut extra = spec.extra.clone();
    match spec.command.as_str() {
        "projects" => Ok(vec![]),
        "snapshot" => {
            if extra.is_empty() {
                extra.push("--keep".into());
                extra.push("2".into());
            }
            Ok(extra)
        }
        "expiry" => {
            if extra.is_empty() {
                extra.push("--days".into());
                extra.push("30,15,3".into());
            }
            Ok(extra)
        }
        "disk" => {
            if extra.is_empty() {
                extra.push("--threshold".into());
                extra.push("90".into());
            }
            Ok(extra)
        }
        other => anyhow::bail!("未知命令: {other:?}（支持: projects | snapshot | expiry | disk）"),
    }
}

/// 拼接子进程 argv（测试友好：纯函数）。
fn build_args(config_dir: &Path, project: Option<&str>, provider: Option<&str>, command: &str, extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "--config".to_string(),
        config_dir.display().to_string(),
        "--log".to_string(),
        "info".to_string(),
    ];
    if let Some(p) = project {
        args.push("--project".into());
        args.push(p.into());
    }
    if let Some(k) = provider {
        args.push("--provider".into());
        args.push(k.into());
    }
    args.push(command.into());
    args.extend(extra.iter().cloned());
    args
}

/// 从 jsonl 加载历史（逐行容错）。
fn load_history(config_dir: &Path) -> Vec<TaskMeta> {
    let path = config_dir.join("ops-console-tasks.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    text.lines()
        .filter_map(|l| serde_json::from_str::<TaskMeta>(l).ok())
        .collect()
}

/// 追加一条历史到 jsonl。
fn append_history(config_dir: &Path, meta: &TaskMeta) -> anyhow::Result<()> {
    let path = config_dir.join("ops-console-tasks.jsonl");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", serde_json::to_string(meta)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_args() {
        let args = build_args(Path::new("/cfg"), Some("demo"), Some("aliyun"), "snapshot", &["--keep".into(), "2".into()]);
        assert_eq!(args, vec![
            "--config".to_string(), "/cfg".to_string(), "--log".to_string(), "info".to_string(),
            "--project".to_string(), "demo".to_string(), "--provider".to_string(), "aliyun".to_string(),
            "snapshot".to_string(), "--keep".to_string(), "2".to_string(),
        ]);
        let args2 = build_args(Path::new("/cfg"), None, None, "projects", &[]);
        assert_eq!(args2, vec![
            "--config".to_string(), "/cfg".to_string(), "--log".to_string(), "info".to_string(),
            "projects".to_string(),
        ]);
    }

    #[tokio::test]
    async fn test_queue_state_machine() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project.yml"),
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n",
        )
        .unwrap();
        let mgr = TaskManager::new(&dir).unwrap();
        let spec = CommandSpec { command: "projects".into(), project: None, provider: None, extra: vec![] };
        let meta = mgr.submit(spec).unwrap();
        assert_eq!(meta.status, "queued");
        // 取一个任务跑：spawn current_exe()（测试环境下是 libtest harness，CLI 参数不被识别，
        // 终态为 failed 属预期）——本测试验证的是队列/子进程/输出落盘/状态机的完整闭环，
        // 真实 CLI 成功路径由任务 6 手动冒烟覆盖。
        mgr.run_next_if_idle().await.unwrap();
        // 等任务结束（current 清空且历史出现终态记录）
        for _ in 0..100 {
            if mgr.current().is_none() && mgr.list().iter().any(|t| t.status == "success" || t.status == "failed") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let hist = mgr.list();
        let done = hist.iter().find(|t| t.status == "success" || t.status == "failed");
        assert!(done.is_some(), "历史应有终态（success/failed）任务: {hist:?}");
        let t = done.unwrap();
        assert_eq!(t.id, meta.id);
        assert!(t.duration_secs.is_some(), "应记录耗时");
        // 输出文件存在且非空（libtest 的错误行被捕获落盘）
        let log_path = dir.join("tasks").join(format!("{}.log", t.id));
        let out = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(!out.is_empty(), "输出文件应非空");
        // 历史持久化到 jsonl
        let jsonl = std::fs::read_to_string(dir.join("ops-console-tasks.jsonl")).unwrap();
        assert!(jsonl.contains(&t.id));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_read_output_rejects_path_traversal() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("tasks")).unwrap();
        // 合法 32-hex id（uuid simple 格式）→ 正常读取
        let id: String = "0123456789abcdef".repeat(2);
        std::fs::write(dir.join("tasks").join(format!("{id}.log")), "task output").unwrap();
        // tasks 目录之外的文件：路径穿越必须被拒绝
        std::fs::write(dir.join("probe.log"), "PROBE").unwrap();
        let mgr = TaskManager::new(&dir).unwrap();
        assert_eq!(mgr.read_output(&id).as_deref(), Some("task output"));
        assert_eq!(mgr.read_output("../probe"), None, "路径穿越应被拒绝");
        assert_eq!(mgr.read_output("../../probe"), None, "路径穿越应被拒绝");
        assert_eq!(mgr.read_output("probe"), None, "非 32-hex id 应被拒绝");
        assert_eq!(mgr.read_output(&format!("{id}/x")), None, "含 / 的 id 应被拒绝");
        assert_eq!(mgr.read_output(&format!("{id}..")), None, "含 .. 的 id 应被拒绝");
        assert_eq!(mgr.read_output(&"g".repeat(32)), None, "非 hex 字符应被拒绝");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_broadcast() {
        // 普通行
        let (id, kind, payload) = parse_broadcast("abc123\tHello world").unwrap();
        assert_eq!(id, "abc123");
        assert_eq!(kind, "line");
        assert_eq!(payload, "Hello world");
        // __status__ 行
        let (id, kind, payload) = parse_broadcast("abc123\t__status__ success").unwrap();
        assert_eq!(id, "abc123");
        assert_eq!(kind, "status");
        assert_eq!(payload, "success");
        let (_, kind, payload) = parse_broadcast("abc123\t__status__ running").unwrap();
        assert_eq!(kind, "status");
        assert_eq!(payload, "running");
        // 无 tab 容错
        assert!(parse_broadcast("no-tab-message").is_none());
        // 空 task_id 容错
        assert!(parse_broadcast("\t__status__ running").is_none());
    }

    #[tokio::test]
    async fn test_broadcast_messages_carry_task_id() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project.yml"),
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n",
        )
        .unwrap();
        let mgr = TaskManager::new(&dir).unwrap();
        let mut rx = mgr.tx.subscribe();
        let spec = CommandSpec { command: "projects".into(), project: None, provider: None, extra: vec![] };
        let meta = mgr.submit(spec).unwrap();
        mgr.run_next_if_idle().await.unwrap();
        // 等待终态广播；期间所有消息必须带任务 id 前缀
        let mut saw_running = false;
        let mut saw_terminal = false;
        for _ in 0..300 {
            match rx.try_recv() {
                Ok(msg) => {
                    assert!(
                        msg.starts_with(&format!("{}\t", meta.id)),
                        "广播消息应带任务 id 前缀: {msg:?}"
                    );
                    let (_, kind, payload) = parse_broadcast(&msg).unwrap();
                    match kind {
                        "status" => {
                            if payload == "running" { saw_running = true; }
                            if payload == "success" || payload == "failed" { saw_terminal = true; }
                        }
                        _ => {}
                    }
                    if saw_terminal { break; }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(_) => break,
            }
        }
        assert!(saw_running && saw_terminal, "应收到 running 与终态广播（saw_running={saw_running}, saw_terminal={saw_terminal}）");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_sse_follows_queued_tasks() {
        // 排队场景：任务 A 运行中提交 B。同一订阅者应依次收到 A 终态与 B 的
        // running/终态（各自带任务 id），B 的事件不得因 A 终态而断流。
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project.yml"),
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n",
        )
        .unwrap();
        let mgr = TaskManager::new(&dir).unwrap();
        let mut rx = mgr.tx.subscribe();
        let spec = CommandSpec { command: "projects".into(), project: None, provider: None, extra: vec![] };
        let a = mgr.submit(spec.clone()).unwrap();
        let b = mgr.submit(spec).unwrap();
        let mut events: Vec<(String, String, String)> = Vec::new();
        for _ in 0..600 {
            match rx.try_recv() {
                Ok(msg) => {
                    if let Some((id, kind, payload)) = parse_broadcast(&msg) {
                        events.push((id.to_string(), kind.to_string(), payload.to_string()));
                        if id == b.id && kind == "status"
                            && (payload == "success" || payload == "failed")
                        {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(_) => break,
            }
        }
        let statuses: Vec<(&str, &str)> = events
            .iter()
            .filter(|(_, k, _)| k == "status")
            .map(|(i, _, p)| (i.as_str(), p.as_str()))
            .collect();
        let idx = |id: &str, st: &str| statuses.iter().position(|(i, s)| *i == id && *s == st);
        assert!(idx(&a.id, "running").is_some(), "A 应有 running: {statuses:?}");
        let a_term = idx(&a.id, "success").or_else(|| idx(&a.id, "failed"));
        assert!(a_term.is_some(), "A 应有终态: {statuses:?}");
        let b_run = idx(&b.id, "running");
        assert!(b_run.is_some(), "B 应有 running: {statuses:?}");
        let b_term = idx(&b.id, "success").or_else(|| idx(&b.id, "failed"));
        assert!(b_term.is_some(), "B 应有终态: {statuses:?}");
        // B 的 running 必须在 A 终态之后（单 worker FIFO），B 终态在 B running 之后
        assert!(b_run.unwrap() > a_term.unwrap(), "B 的 running 应在 A 终态之后: {statuses:?}");
        assert!(b_term.unwrap() > b_run.unwrap(), "B 的终态应在 B running 之后: {statuses:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_finish_emits_terminal_even_if_history_append_fails() {
        let dir = std::env::temp_dir().join(format!("ops-console-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("project.yml"),
            "- name: demo\n  providers:\n    aliyun:\n      region: cn-shenzhen\n",
        )
        .unwrap();
        // 让 append_history 必然失败：在 jsonl 路径上创建目录（Windows/Unix 打开目录写均报错）
        std::fs::create_dir(dir.join("ops-console-tasks.jsonl")).unwrap();
        let mgr = TaskManager::new(&dir).unwrap();
        let mut rx = mgr.tx.subscribe();
        let spec = CommandSpec { command: "projects".into(), project: None, provider: None, extra: vec![] };
        let meta = mgr.submit(spec).unwrap();

        // 等待终态广播（libtest 子进程跑 CLI 会失败，但无论 success/failed 终态事件都必须发出）
        let mut terminal: Option<String> = None;
        for _ in 0..200 {
            match rx.try_recv() {
                Ok(line) => {
                    if let Some((_, kind, payload)) = parse_broadcast(&line) {
                        if kind == "status" && (payload == "success" || payload == "failed") {
                            terminal = Some(payload.to_string());
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(_) => break,
            }
        }
        let terminal = terminal.expect("append_history 失败时仍应广播终态事件");
        assert!(terminal == "success" || terminal == "failed", "终态应为 success/failed: {terminal}");

        // store.current 必须被清空，且历史中存在该任务的终态记录
        for _ in 0..100 {
            if mgr.current().is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(mgr.current().is_none(), "终态后 current 应被清空");
        let hist = mgr.list();
        let done = hist.iter().find(|t| t.id == meta.id);
        assert!(done.is_some(), "历史应有该任务记录: {hist:?}");
        assert_eq!(done.unwrap().status, terminal);
        std::fs::remove_dir_all(&dir).ok();
    }
}
