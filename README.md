# ops-console

服务商运维系统：多服务商统一运维操作 CLI（Rust）。

起步实现：**阿里云轻量应用服务器（SWAS）快照轮转**。通过服务商抽象层，后续可平滑接入腾讯云、AWS 等。

## 功能

- 多项目 × 多服务商配置（`config/project.yml`），一条命令遍历全部项目/服务商
- 阿里云轻量服务器快照轮转：删旧建新，保留指定份数，自动等待就绪
- 通知渠道抽象：钉钉机器人（加签 + 标题签名），可替换扩展
- 凭据支持配置文件与环境变量双重注入（推荐环境变量，适配 CI / systemd / cron）

## 快速开始

```bash
# 构建
cargo build --release

# 准备配置（从示例复制后填写）
cp config/project.yml.example config/project.yml
cp config/notify.yml.example config/notify.yml   # 可选，不需要通知可省略

# 查看项目
./target/release/ops-console projects

# 快照轮转（保留 2 份）：全部项目 × 全部服务商
./target/release/ops-console snapshot --instance <instance-id> --keep 2

# 指定项目 / 指定服务商
./target/release/ops-console --project demo snapshot --instance <id> --keep 2
./target/release/ops-console --provider aliyun snapshot --instance <id> --keep 2
```

## 配置

### project.yml（项目与服务商）

YAML 文档根即项目数组，每个项目下 `providers.<kind>` 配置服务商，`key` 即服务商类型（`aliyun` / 未来的 `tencent` / `aws` ...）。

```yaml
- name: demo
  description: 示例项目
  providers:
    aliyun:
      region: cn-shenzhen
      access_key_id: ""        # 留空则用环境变量
      access_key_secret: ""

- name: prod
  providers:
    aliyun:
      region: cn-hangzhou
      access_key_id: ""
      access_key_secret: ""
```

### notify.yml（通知渠道，可选）

不存在此文件 = 不通知。

```yaml
kind: dingtalk
prefix: "【通知】"        # 标题签名，标记消息来源；空 = 不加

dingtalk:
  webhook: ""              # 留空则用环境变量 DINGTALK_WEBHOOK_URL
  secret: ""               # 加签密钥，留空则用环境变量 DINGTALK_SECRET
```

### 环境变量

| 变量 | 作用 |
|---|---|
| `ALIYUN_ACCESS_KEY_ID` / `ALIYUN_ACCESS_KEY_SECRET` | 覆盖所有项目的阿里云凭据 |
| `DINGTALK_WEBHOOK_URL` / `DINGTALK_SECRET` | 覆盖钉钉通知 webhook / 加签密钥 |

## 命令参考

```text
ops-console [--config <目录>] [--project <名>] [--provider <kind>] snapshot [选项]

全局参数:
  --config <目录>    配置目录（含 project.yml / notify.yml），默认 config/
  --project <名>     目标项目，默认全部项目
  --provider <kind>  只执行指定服务商（如 aliyun），默认项目内全部服务商
  --log <级别>       error|warn|info|debug，默认 info

子命令:
  projects                 列出所有项目
  snapshot --instance <id> [--keep 2] [--wait-minutes 30]
                          快照轮转：遍历目标项目 × 服务商，删旧建新
                          单项目/服务商失败不阻断其余，最后汇总报错并退出非零
```

## 快照轮转策略

1. 校验 `keep >= 1`；按创建时间排序
2. 删除多余的旧快照，保留最新 `keep-1` 份（新建 1 份后合计 `keep` 份）；删除失败仅告警不中断
3. 存在创建中的快照则轮询等待（30s 间隔，超时上限 `--wait-minutes`），避免超过单台上限
4. 清理 Failed 快照（占名额且无用）
5. 创建新快照（命名 `{服务器名}-{北京时间 YYYYMMDD-HHMMSS}`），等待变为可用
6. 输出摘要，并发送到通知渠道（发送失败仅告警，不影响退出码）

> 阿里云轻量限制：单台最多 3 个快照。`keep=2` 时轮转窗口 = 2 旧 + 1 新建中。

## 阿里云 RAM 权限

最小权限策略（注意 Action 前缀是 `swas-open:` 而非 `swas:`）：

```json
{
  "Version": "1",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "swas-open:ListInstances",
        "swas-open:ListDisks",
        "swas-open:ListSnapshots",
        "swas-open:CreateSnapshot",
        "swas-open:DeleteSnapshot"
      ],
      "Resource": "*"
    }
  ]
}
```

## cron 示例

```bash
# 每日 03:00 轮转全部项目，钉钉通知（凭据走环境变量，不经文件）
0 3 * * * DINGTALK_WEBHOOK_URL=... DINGTALK_SECRET=... \
  /opt/ops-console/target/release/ops-console \
  --config /opt/ops-console/config \
  snapshot --instance <id> --keep 2 >> /var/log/ops-console.log 2>&1
```

## 架构

```mermaid
graph LR
    CLI[main.rs: clap CLI] --> C[config.rs: 项目/服务商/通知配置]
    CLI --> P[ops/snapshot.rs: 轮转业务逻辑]
    P --> T[cloud::CloudProvider trait]
    T --> A[cloud/aliyun: 阿里云实现]
    P --> N[notify::Notifier trait]
    N --> D[notify/dingtalk: 钉钉实现]
```

- **服务商抽象**：`cloud::CloudProvider` 定义统一的 `Server` / `Snapshot` 模型，轮转逻辑只依赖 trait。接入新服务商 = 实现 trait + 一个 API 模块（参考 `cloud::aliyun`）。
- **通知抽象**：`notify::Notifier` 统一 `send(title, text)`。接入 Slack / Telegram 等 = 实现 trait + 在 `notify::from_config` 加一个分支。
- **签名复用**：`cloud/aliyun/sign.rs` 是通用阿里云 RPC V3 签名（HMAC-SHA1），SWAS / ECS / SLB / DNS 等任意 RPC 风格产品换 endpoint + Version + Action 即可复用。

## 发布（Release）

推送 `v*` tag 即触发 GitHub Actions 自动发布：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

- 在 `ubuntu-latest` 上用 `x86_64-unknown-linux-musl` 交叉编译**静态 Linux 二进制**（不依赖 glibc，任意发行版可直接运行）
- 产物：`ops-console-<版本>-linux-x86_64.tar.gz`，随 GitHub Release 发布
- Release Notes 由 `generate_release_notes` 自动生成（基于 commit）

## 开发

```bash
cargo check     # 编译检查
cargo test      # 单元测试（签名向量、通知消息体）
```
