# ops-console Web UI 设计规格

日期：2026-08-10
状态：已批准（用户逐项确认）

## 1. 目标

为 ops-console 提供 Web 管理界面，实现两件事：

1. **直接管理项目配置**：以表单为主、YAML 原文为辅，管理 `config/project.yml` 与 `config/notify.yml`
2. **手动运行子命令**：从浏览器选择项目/服务商/参数，运行 `projects` / `snapshot` / `expiry` / `disk`，流式查看输出并保留历史

## 2. 已确认的决策

| # | 决策点 | 选择 |
|---|---|---|
| 1 | 运行形态 | 内置到 Rust 二进制，新增 `serve` 子命令（axum） |
| 2 | UI 框架 | Bootstrap 5 纯 HTML，**无构建步骤** |
| 3 | Bootstrap 资源 | 本地 vendor 文件打进二进制，离线可用 |
| 4 | 认证 | 简单 Token 认证，监听地址可配置（默认 `127.0.0.1:8899`） |
| 5 | 配置管理 | 表单为主 + YAML 原文"高级模式" |
| 6 | 任务 | 单 worker 队列 + 持久化历史 |
| 7 | 执行方式 | spawn 自身二进制（`current_exe()`）子进程，与 CLI 行为 100% 一致 |
| 8 | Token 存储 | `config/serve.yml`，空则自动生成并写回 |

## 3. CLI 层：`serve` 子命令

```
ops-console serve [--addr 127.0.0.1:8899] [--token <token>]
```

- `--addr`：监听地址，默认 `127.0.0.1:8899`
- `--token`：访问令牌，优先级最高

### 3.1 Token 解析优先级

1. `--token` 参数（命令行）
2. 环境变量 `OPS_CONSOLE_TOKEN`
3. `serve.yml`（位于 `--config` 指定的目录）中的 `token` 字段
4. 以上皆无 → 生成随机 token（32 字节 hex），**写回 `serve.yml` 保存**，并在终端打印提示

### 3.2 serve.yml

```yaml
# 由 ops-console serve 自动维护；token 为空时自动生成并保存
token: ""
```

- 保存时以 Unix 0600 权限写入（含敏感凭据）
- 仅 `token` 一个字段（监听地址由 `--addr` 参数控制，不入文件）

### 3.3 新增依赖

- `axum`（HTTP 框架，含 `tokio` 已有）
- 静态资源：`include_str!` 内嵌（index.html + vendor bootstrap.min.css/bootstrap.bundle.min.js + app.js）

## 4. HTTP API

除 `/api/login` 外全部要求 `Authorization: Bearer <token>` 头；token 校验失败返回 401。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | index.html |
| GET | `/static/*` | vendor 静态资源 |
| POST | `/api/login` | `{token}` → 校验，成功返回 `{ok:true}` |
| GET | `/api/config` | 结构化配置（项目/服务商/通知），secret 字段掩码 |
| POST | `/api/config` | 保存表单配置（服务端序列化 YAML + 校验） |
| GET | `/api/config/raw` | `{project_yml, notify_yml}` 原文 |
| POST | `/api/config/raw` | 保存原文（解析校验 + 至少 1 个项目） |
| POST | `/api/run` | `{command, project?, provider?, keep?, wait_minutes?, days?, threshold?}` → `{task_id}` |
| GET | `/api/tasks` | 历史任务列表（含状态/退出码/耗时） |
| GET | `/api/tasks/current` | 当前运行/排队任务状态 + 输出尾部 |
| GET | `/api/tasks/current/stream` | SSE 流式输出（EventSource） |
| GET | `/api/tasks/{id}/output` | 指定历史任务的完整输出 |

### 4.1 参数映射

- 全局参数：`--config <serve 启动时用的目录>`、`--project`（下拉：全部/各项目）、`--provider`（下拉：全部/项目内服务商）、`--log info`（固定）
- 命令参数（前端按命令动态渲染表单）：
  - `snapshot`：`--keep`（默认 2）、`--wait-minutes`（默认 30）
  - `expiry`：`--days`（默认 `30,15,3`）
  - `disk`：`--threshold`（默认 90）
  - `projects`：无参数

## 5. 任务执行与历史

### 5.1 队列

- 单 worker：同一时刻最多 1 个任务，新任务进入 `queued`
- 状态机：`queued → running → success | failed`
- `failed` = 子进程退出码非零或 spawn 失败；`success` = 退出码 0
- 用 tokio `mpsc` 入队 + 共享状态（Mutex）保存当前任务句柄

### 5.2 执行

- `std::process::Command` spawn `current_exe()`，参数：`--config <dir> [--project <p>] [--provider <k>] --log info <command> [args...]`
- stdout/stderr 合并（`2>&1` 重定向到同一管道），逐行读取 → SSE 广播 + 落盘
- 环境变量继承（凭据可通过环境变量注入，与 CLI 行为一致）
- 当前工作目录 = 启动 serve 时的工作目录

### 5.3 历史持久化

- 元数据：`<config_dir>/ops-console-tasks.jsonl`（追加行，每条 = task 元数据：id/时间/命令/参数/状态/退出码/耗时/输出文件路径）
- 完整输出：`<config_dir>/tasks/<task_id>.log`
- serve 重启后历史仍可查询（扫描 jsonl + 按需读 log）
- 索引与 jsonl 不同步时的处理：以 jsonl 为准，缺失的 log 显示"输出文件缺失"

### 5.4 并发与取消

- 不提供取消操作（YAGNI，V1 不做）

## 6. 配置管理

### 6.1 表单模式

- 项目 CRUD：名称、描述；新增/删除项目
- 服务商 CRUD：`region` / `access_key_id` / `access_key_secret`；每项目可加多个服务商（kind 如 aliyun）
- secret 字段默认掩码显示（`••••••••`），可切换明文
- 通知设置：`kind`（none/dingtalk）、`prefix`、`webhook`、`secret`
- 保存 → POST `/api/config` → 服务端序列化为 YAML（保留现有 `Config` 结构形状：项目数组 + notify 块）→ 校验通过后写盘

### 6.2 YAML 高级模式

- 页面右上角切换：`project.yml` / `notify.yml` 两个 textarea
- 保存 → POST `/api/config/raw` → 先解析校验（复用 `Config::from_str`）→ 通过后写盘

### 6.3 校验与错误处理

- `config.rs` 重构：把 `Config::load` 的文件读取与解析拆开，抽出 `Config::from_str(project_yml: &str, notify_yml: Option<&str>) -> Result<Config>` 供保存时校验复用
- 校验规则：YAML 可解析；项目至少 1 个；项目名非空
- 保存失败 → 返回 400 + 错误消息（含解析错误的行号/信息），**不写盘**；前端保留用户输入并展示错误
- 表单模式与 YAML 模式可能互相覆盖：保存哪个文件就写哪个文件（project.yml 与 notify.yml 独立保存）

### 6.4 GET /api/config 的 secret 掩码

- 结构化返回中 `access_key_secret` / `secret` 用掩码字符串（如 `"••••••••"`）替代
- 掩码仅用于展示；表单提交时若字段值为掩码（未改动），服务端保留原值不覆盖；用户显式输入新值才覆盖
- 实现方式：`POST /api/config` 时若字段 == 掩码标记 → 跳过该字段更新（读回现有文件值）

## 7. 前端结构

单页应用（Bootstrap 5，无构建），一个 `index.html` + `app.js` + vendor 文件，全部内嵌进二进制。

### 7.1 页面

1. **登录页**：token 输入框 → POST /api/login → 成功存 localStorage，失败提示
2. **运行命令页**（默认首页）：
   - 左侧：命令下拉（projects/snapshot/expiry/disk）+ 项目下拉 + 服务商下拉 + 动态参数表单 + 运行按钮（运行中禁用）
   - 右侧：终端风格输出面板（黑底等宽字体），SSE 流式追加，顶部状态徽标（排队中/运行中/成功/失败）
   - 任务结束后保留输出，可一键"再次运行"（同参数重发）
3. **项目配置页**：
   - 左侧：项目列表 + 新增项目 + 通知设置入口
   - 右侧：选中项目的编辑表单（名称/描述/服务商列表/添加服务商/删除服务商/删除项目）
   - 右上角："切换到 YAML 原文"（textarea 双文件编辑）
   - 底部：保存按钮 + 保存结果提示
4. **运行历史页**：
   - 表格：时间 / 命令 / 参数 / 状态（徽标）/ 耗时 / 操作（查看输出）
   - 查看输出：模态框展示完整 log（等宽字体）

### 7.2 导航

顶部 navbar：`ops-console` 品牌 + 三个页签。用 hash 路由（`#/run`、`#/config`、`#/history`）实现单页切换，无外部依赖。

## 8. 错误处理汇总

| 场景 | 行为 |
|---|---|
| token 无效/缺失 | 401，前端跳登录页 |
| 配置保存校验失败 | 400 + 错误消息，不写盘，保留输入 |
| 任务入队时无空闲 | 排队（前端显示"排队中"） |
| 子进程 spawn 失败 | 任务标记 failed，输出记录错误 |
| 历史 log 文件缺失 | 显示"输出文件缺失" |
| 静态资源 | 全部内嵌，无网络依赖 |

## 9. 测试

- `serve` token 解析：`--token` 覆盖 env/serve.yml；serve.yml 空 → 生成并写回（0600 权限断言）
- `Config::from_str` 校验：合法配置通过；空项目/坏 YAML 拒绝
- 配置保存：POST /api/config 掩码字段不覆盖；表单序列化 → 重新解析一致
- 任务状态机：queued → running → success/failed（用假的短命令验证，如 `projects`）
- 子进程参数拼接：单元测试构造 Command 参数序列
- HTTP 层：用 axum 的测试客户端（tower::ServiceExt::oneshot）验证路由 + 401 路径

## 10. 范围外（YAGNI）

- 取消/终止正在运行的任务
- 多 worker 并发执行
- 任务定时/循环（cron）调度
- 用户管理/多角色
- 日志在线查看（tail -f 类）
- 国际化
