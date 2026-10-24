# macOS 原生开发与运行

当前入口是原生 Rust CLI 与后台守护。**聊天和任务都通过 DeepSeek 生成，本地只运行 Laya 决策与 EmbeddingGemma 2 检索辅助服务。**
Apple Silicon 使用本机 arm64 构建；通知接入通知中心，登录自启接入当前用户的 launchd LaunchAgent。

## 准备环境

需要 Xcode Command Line Tools、Rust 1.88+ 和 Python 3.12。在仓库根目录执行：

```bash
./安装Mac环境.command
./启动.command chat
```

默认安装脚本创建辅助模型使用的 `.venv`、下载校验 Laya 并构建 release，不再安装 MLX 或下载生成模型。
使用 `YUNXI_PYTHON=/绝对路径/python3.12` 选择解释器。Windows venv 与构建产物不能直接复用。
启动器默认数据目录是仓库的 `data/`，尊重已有的 `YUNXI_BOT_HOME`；直接运行 Rust CLI 默认仍是 `~/.yunxi-bot`。

DeepSeek 凭证放在该数据目录的 `secrets/deepseek.key`（Mac 权限 0600），或设置环境变量 `YUNXI_BOT_DEEPSEEK_KEY`。
聊天、任务、摘要、画像提炼和主动陪伴均需要它。启动脚本与 LaunchAgent 不保存密钥。
生成时会发送相应的人格、画像、召回内容和工具结果；本地保留源资料、向量缓存与台账。

## 启动与检查

```bash
./启动.command status
./启动.command chat list       # 只列会话，不启动模型
./启动.command chat
./启动.command do "读取 README 并总结项目用途"
./启动.command chat --thinking on
./启动.command chat --thinking off
curl http://127.0.0.1:17870/health
```

聊天先由 Laya 分类：聊天走本机 MLX（默认 Qwen3.5-9B，可切 Gemma 4 26B），任务走 `deepseek-flash`；两条链路使用不同系统提示词与独立会话。App 启动 `chat-server` 时带 `--auto-local`，但**第一次聊天才加载本机模型**；无输入 10 分钟后自动释放显存（进程保留，`YUNXI_LOCAL_IDLE_UNLOAD` 可改）。窗口副标题显示“本机 · 模型名”，无需先手动开终端。工作代理只读 `data/work/` 的工作人格、工作画像、工作记忆与工作知识库，不读陪伴的画像/记忆/私聊历史；`./启动.command work ...` 管理这些文件。
分类失败会停止本轮。默认闲聊不开思考，任务规划/分析按类型开启，显式开关优先。远端失败不会自动启动本地生成权重。
Laya 通过 PyTorch MPS 运行；向量编码器使用 CPU。`do/resume --provider local` 已拒绝，避免轻量模式重新加载大模型。

知识检索按需启动 `127.0.0.1:17873` 的 EmbeddingGemma 2 文本服务，复用 `.venv`。
本机权重已下载；新机器可运行：

```bash
.venv/bin/python scripts/fetch_embedding_candidates.py --only embeddinggemma-2
./启动.command knowledge add /完整路径/资料.md
./启动.command knowledge search "需要查询的内容"
```

首个需要向量的任务或搜索自动拉起服务，日志在数据目录 `embedding-sidecar.log`。加载失败明确报告。
任务上下文和知识库边界见 [任务上下文与知识库](任务上下文与知识库.md)，当前实测见 [轻量运行架构](轻量运行架构.md)。

Gemma/Qwen 现在是**陪伴代理的默认生成后端**。使用前运行 `./启动本地模型.command`（默认 Qwen3.5-9B；`YUNXI_COMPANION_MODEL=gemma` 切 Gemma 4 26B）。本机模型未启动时聊天会明确报错，不会回落到云端。

## 通知中心

日历、提醒事项和模型可调用的通知工具见 [Mac 本机助理工具](Mac本机助理工具.md)。先构建原生桥，再使用 `mac authorize calendar` / `mac authorize reminders` 授权。

```bash
./启动.command mac authorize notifications
./启动.command notify "云熙" "macOS 通知接口检查"
```

使用“云熙本机助理”自己的 `UNUserNotificationCenter`，无第三方通知依赖。`mac status` 返回独立通知权限和设置；只有回读到匹配的通知中心记录才返回 `Confirmed`（退出码0），仅接受但未回读到返回 `HandedOff`（退出码3）。两者都不证明横幅实际可见或用户已读。相同tag和内容在通知中心中已存在时不重复投递，专注模式和系统通知设置仍生效。需要在终端查看时加 `--console`。

Mac上的 `mail`、`check`、`daemon` 默认使用本机Mail。数据目录 `mac-mail.json` 为账户和收件箱路径数组，例如 `[{"account_id":"mac_mail_accounts返回的ID","mailbox_path":["INBOX"]}]`；不自动纳入草稿、归档或本地合成邮箱。不会把邮件标为已读。显式 `--mail-source imap` 或 `--port` 仍可使用原有IMAP sidecar；来源不通时报错，不切换账户或伪装为空。权限与本机同步延迟会影响收件发现时间。

## 登录自启

```bash
# 查看将要安装的配置，不修改系统：
./启动.command install-autostart --dry-run -- --agent

# 注册并立即交给 launchd 运行：
./启动.command install-autostart -- --agent
./启动.command autostart-status

# 停止服务并取消下次登录自启，保留数据：
./启动.command uninstall-autostart
```

配置为 `~/Library/LaunchAgents/com.yunxi.bot.agent.plist`。launchd 直接运行 daemon，保留安装时的绝对程序路径、工作目录和数据目录；启动参数在 `--` 后传入，不经 shell 解析。后台使用默认决策端点时会自行拉起 Laya。自定义端点由用户管理。

日志位于 `$YUNXI_BOT_HOME/logs/daemon.stdout.log` 和 `daemon.stderr.log`。异常退出由 launchd 限速重启，正常退出不反复重启。仓库移动后应重新注册，以更新程序和工作目录路径。邮件来源使用同一数据目录中的 `mac-mail.json`；IMAP来源仍使用已有sidecar配置。

## 开发验证

```bash
cargo fmt --all -- --check
cargo build --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
.venv/bin/python -m unittest discover -s sidecar -p 'test_*.py'
.venv-mlx/bin/python -m unittest discover -s sidecar -p test_mlx_backend.py
.venv/bin/python -m unittest discover -s scripts -p 'test_*.py'
```

回归测试覆盖陈旧锁、真实子进程退出、Unix 进程组超时、继承管道、目录别名和越界符号链接、平台 Python 选择、模型设备选择、通知结果语义与 launchd 配置/失败恢复。

Mac 的执行隔离仍为进程级控制；没有宣称 OS 目录写入沙箱。原有审批、台账格式和决策失败方向保持原契约。

### 历史快照：首次 Mac 适配验收（2026-10-15）

本节及下方模型验收记录保留各阶段结果；当前部署以 README 和 2026-10-16 的工具/邮件验收文档为准，旧阶段的权限、启动状态和模型选型不是当前状态。

Apple M2 Max 上已构建 arm64 release，并建立 Python 3.12 项目环境。Rust 1,067 项测试、Python 64 项测试通过；格式、构建与严格 Clippy 检查通过。

PyTorch 2.14.1 / Transformers 5.19.0 实测自动选择 MPS，以小型随机权重 Qwen3 完成 HTTP 普通响应、SSE 流式响应、卸载及按需重载。这验证了设备与推理链路，不代表正式 Qwen 4B 权重的效果或性能。正式 Qwen 权重、Windows 旧数据和凭证尚未迁移。

系统通知返回 HandedOff；临时 LaunchAgent 已完成真实 bootstrap、单轮 daemon 运行与 bootout 清理。长期登录自启尚未启用。


### Laya 接入验收（2026-10-15）

默认决策后端已按用户选择切换为 `laya-multilingual`。本机已下载约 678.2 MB
模型文件并校验清单中的 SHA256；在 `HF_HUB_OFFLINE=1` 与
`TRANSFORMERS_OFFLINE=1` 下完成 MPS 加载、预热及 HTTP 推理。

短中文输入同时提出 choice / score / noul 三类问题，首个请求约 952 ms，随后
4 次约 23–25 ms。这只是该请求的链路测量，不能外推长输入、并发或中文准确率。
Rust release CLI 的 `decide` 实测得到 `model: laya-multilingual`、
`degraded: false`；超长输入触发截断后返回 503，让调用方显式降级。

本次验收通过 Rust 1,067 项、Python sidecar 77 项和下载器 4 项测试，
格式、构建、Clippy 与 `pip check` 通过。另修复下载器设置 NO_PROXY 后
遮蔽 macOS 系统代理的问题。安装脚本默认下载 Laya；`--check` 会重算已记录的哈希。

模型仍为基础权重，`calibrated: false`。首批 16 条中文场景待用户审核，
尚未进行训练或校准。历史 Verdict 可通过 `sidecar/requirements-verdict.txt`
安装，并用 `scripts/fetch_model.py --only verdict-small` 准备对比权重。


### 历史快照：Gemma + Laya 联合验收（已由轻量模式替代）

已用本机正式权重通过以下检查：

- `启动.command chat list` 从无服务状态自动拉起两个模型；启动器退出后服务仍存活，
  Gemma 的 health 为 `ok / mlx`，Laya 为 `ok / mps / calibrated:false`。
- Gemma 普通聊天、开启思考、非流式与 SSE 原生工具调用、工具结果回传、
  `/unload` 后的按需重载均通过。
- release bot 在临时数据目录完成闲聊、真实 `now` 调用和中文路径 `read_file`；
  五次生成均记录为 `provider:local`、Gemma 模型名，没有借用远端生成服务。
- 最终 release 再验 `--thinking on`；输入分类与记忆决策均记录实际 Laya 名称，
  `degraded:false`。命中 `--deny write_file` 的调用记录 `executed:false`，文件未创建。
- Rust 1,071 项测试、MLX 环境 sidecar 90 项测试、scripts 16 项测试通过；
  debug/release 构建、格式检查、严格 Clippy、两个 venv 的 `pip check` 通过。
  邮件既有测试仍出现非失败的 socket ResourceWarning。

验收记录保存在 `data/benchmarks/gemma-laya-integration.json`、
`gemma-laya-cli-integration.json` 与 `gemma-laya-final-acceptance.json`。中间 CLI
记录保留了输入分类曾误标 Verdict 的证据；最终记录已验证修复，不改写旧审计记录。

验收时系统交换用量保持 0.25 MB，Qwen3.5-9B 未加载。这是功能联调与回归，
尚未做长期驻留、休眠唤醒或 Windows 真机验收，也未开始微调/校准。

## 任务链路

`do`、`resume` 和 daemon 中的目标任务统一使用 DeepSeek，原有任务可以沿用 ID 续跑；
有待决选择时加 `--answer "答案"`。`--provider local` 已拒绝。
当前链路、失联恢复和工具错误的实测见 [轻量运行架构](轻量运行架构.md)。
