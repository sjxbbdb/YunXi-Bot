# Mac 本机助理工具

第一批覆盖Mac日历、提醒事项和即时通知；第二批增加联系人、邮件、地点、交通、天气和文本/PDF读取，见[第二批工具及验收](第二批工具验收-20261016.md)。用户已明确“只开发Mac端，可使用现有iCloud账户”；日历与待办写入限定iCloud下专用的“云熙”列表，系统可以同步到其他设备。邮件使用Mac Mail现有账户，并受单独的动作审批约束。复用Rust工具注册、审批、执行与台账；不改模型、不训练、不增加运行时依赖。原生应用使用Apple系统SDK，通过原有进程边界访问接口。

## 安装与授权

要求 macOS 14+、Xcode Command Line Tools。已有项目只需：

```bash
bash scripts/build-macos-tools.sh
cargo build --release
./启动.command mac status
./启动.command mac authorize calendar
./启动.command mac authorize reminders
./启动.command mac authorize contacts
./启动.command mac authorize mail
./启动.command mac authorize notifications
```

权限属于 `云熙本机助理.app`（bundle ID `local.yunxi.bot.native`），弹窗必须由使用者在系统中允许。未授权、被拒绝、权限被撤销时，工具返回明确错误；后台任务不会反复弹权限框。已拒绝时请到“系统设置 → 隐私与安全性”中的日历、提醒事项、通讯录、自动化相应页面处理。通讯录只允许部分联系人时，结果明确标记访问受限。

应用位于 `target/macos/云熙本机助理.app`，使用本机 ad-hoc 签名，无第三方运行时。清理 target 后需重建；重新签名或移动项目后系统可能需要重新授权。完整安装脚本已包含构建步骤。

## 第一批工具与边界

| 工具 | 用途 |
|---|---|
| `mac_status` | 日历、提醒事项、通讯录、Mail权限，iCloud存储和通知出口状态，不读取正文 |
| `mac_calendar_list` | 读取指定时间范围（最多 31 天）的日程，含系统中已同步的账户 |
| `mac_calendar_create` | 在 iCloud“云熙”日历创建日程，返回实际读回的记录 |
| `mac_calendar_update` | 修改标题、说明、起止时间，要求刚读取的版本 |
| `mac_calendar_delete` | 删除 Bot 创建的记录，核实已不存在 |
| `mac_reminders_list` | 默认读取未完成待办，可包括已完成事项；支持按备注前缀过滤后计数 |
| `mac_reminder_create` | iCloud“云熙”列表创建待办，可设到期时间与系统提醒 |
| `mac_reminder_update` | 修改文本、延期、完成/恢复；`due_at:null` 清除日期和提醒 |
| `mac_reminder_delete` | 删除 Bot 创建的待办；完成应使用 update |
| `mac_notify` | 复用 macOS 通知出口，立即提交通知 |

写入只接受该实体对应的唯一 **iCloud source**，不回退到默认账户、Google 或其他账户。第一次写入可建立 iCloud“云熙”日历/列表。没有唯一可用账户则返回 `account_unavailable`。只有该列表中带 Bot 来源标记的记录可修改或删除。其他日程、重复事项和带参与者的会议不在首版写入范围。用户手动移动或重命名专用列表后，写入可能被管理范围校验拒绝，需要先检查列表。

3 个查询工具为只读，其余 7 个走显式人工批准或用户明确的 `--allow` 规则，Laya 不能代替批准。没有添加默认放行规则。系统访问授权不会取消 Bot 的动作审批。

时间必须带时区并精确到秒。查询最多返回 200 条，正文有长度上限；返回 `truncated` / `content_truncated` 时不能声称看到了完整资料。查询结果沿用现有 DeepSeek 工具链，会进入模型上下文与任务记录；“本机写入”不意味着生成模型离线。

## 调整与重试

- 修改/删除使用 `id` 和 `expected_revision`，版本变化则报 `conflict`，要求重新读取，不能覆盖用户的新改动。
- 创建必须传唯一 `request_id`，同一次操作重试沿用。相同编号不同参数报错；已经删除的创建结果不会被重试自动复活。
- 创建前记录待定回执，系统保存后记录真实 ID。若在保存与记录回执之间中断，重试报 `outcome_unknown`，先查询核对，不能盲目重复创建。
- 不同 Bot 进程的系统写入用文件锁串行。系统应用中的并发修改不受该锁控制；版本检查发生在保存前，EventKit 未提供跨应用的原子比较交换。
- 修改成功、删除成功分别以读回记录、查询不存在为依据。原生进程与调用方均有超时；超时不能当成功。
- 创建回执位于 `~/Library/Application Support/YunXiBot/native/`（目录 0700），仅含参数摘要和系统 ID；与每个 Bot 数据目录独立，避免不同任务进程重复创建同一操作。请求交换目录 0700、文件 0600，用后清理。

通知由“云熙本机助理”自己的 `UNUserNotificationCenter` 发送。`mac status` 可检查独立通知权限、提醒样式、声音及通知中心开关；关闭权限时到“系统设置 → 通知 → 云熙本机助理”处理。回读到相同标识和内容返回 `confirmed`，系统只接受但未回读到则为 `handed_off`。两者均不证明用户已读或横幅可见；同标签同内容在通知中心中已存在时不重复投递。专注模式和设备休眠仍会影响提醒体验。邮件自动判断与巡检见[邮件原生通知验收](邮件原生通知验收-20261016.md)。

## 使用示例

在 `chat` 或 `do` 中提出：

- “查看今天日历，找一个空闲的小时。”
- “明天上午十点到十一点安排写报告，写入iCloud云熙日历。”
- “明天九点提醒我带材料。”
- “把云熙列表里的带材料待办延后一小时。”
- “把带材料标记完成。”

整天安排与追加重排由 `mac_agenda_plan` / `mac_agenda_apply` 完成，详见[日程安排](日程安排.md)。第三批增加 `mac_trip_plan` / `mac_trip_apply`，多日预排、保存及预订资料用法见[出行住宿与多日行程](出行住宿与多日行程.md)。当前总数48，其中Mac工具36个（16个只读、3个联网、17个写操作）；通用只读工具 `mail_review_pending` 查看邮件摘要及暂缓原因，`include_notified=true` 可回查已通知邮件。Mac只读工具 `mac_mail_context` 关联真实邮件串与已有日程、待办，使用及验收见[邮件与Mac联动](邮件与Mac联动-20261016.md)。休眠唤醒补偿和持续监听系统修改尚未实现。跨设备同步由系统iCloud完成，本机保存成功不代表云端同步已完成。

第二批可直接说“查找张经理的邮箱，有同名先问我”“读取这封邮件的完整往来并保存附件”“起草回复，先不要发送”“明天三点开会，走路过去，准备15分钟，把路途一起安排”。草稿保存在云熙本机，发送前才交给Mail，不能声称已保存到Mail草稿邮箱；实际发送仍需明确审批。[第二批验收报告](第二批工具验收-20261016.md)列出发送、附件、邮件串完整性和联系人测试的限制。

## 验证

```bash
cargo fmt --all -- --check
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
bash scripts/build-macos-tools.sh  # 包含无需权限的原生参数校验测试

# 以下会创建并清理专用测试记录，需要上述系统授权：
cargo test tool::macos::tests::live_ -- --ignored --test-threads=1

# 真实 DeepSeek 调用入口；仅使用合成数据：
.venv/bin/python sidecar/e2e_macos_tools.py --output /tmp/mac-entry-new.json
.venv/bin/python sidecar/e2e_macos_tools.py --lifecycle-only --output /tmp/mac-lifecycle-new.json
```

普通测试覆盖非法时间、越界参数、版本必填、布尔值校验、写入审批、拒绝优先、异常回执和临时文件权限。手动真机测试分别验证日历和提醒事项的创建、读回、修改、重复请求、过期版本拒绝和删除；通知只验证交付语义。

接口依据：[Apple EventKit](https://developer.apple.com/documentation/eventkit/accessing-the-event-store)、[macOS 权限控制](https://support.apple.com/guide/security/protecting-app-access-to-user-data-secc01781f46/web)。

## 本机验收记录（2026-10-16）

- Mac 已授予当前“云熙本机助理”日历、提醒事项的完整访问权限。
- iCloud 下已建立“云熙”日历及“云熙”提醒事项列表；Calendar UI 已核实名称。写入没有落到 Google、个人或工作日历。
- Rust 常规测试 **1,097 项通过**；3 项需真实权限的测试另行运行，**3/3 通过**。
- 原生桥参数校验 **9 项通过**，格式、debug/release 构建、严格 Clippy 与 diff 检查通过。启动器实际使用的 release 已更新。
- 真实 DeepSeek 入口：状态检查、通知两项 **12 个检查通过**；日历/待办各创建→修改→删除，**13 个检查通过**，模型确实调用了全部 6 个写入工具，并使用相同记录 ID 和更新后的版本。全部合成记录已清理，保留专用列表供后续使用。
- 首次系统授权等待超时、应用更名后旧授权未继承，都如实返回失败；这些尝试未创建日程/待办，未计入成功结果。当前构建已重新授权并通过验收。

原始结果位于本机忽略目录：

- `data/benchmarks/tools/mac-native-model-final-20261016.json`
- `data/benchmarks/tools/mac-native-lifecycle-model-20261016.json`
- `data/benchmarks/tools/mac-native-live-20261016.txt`
- `data/benchmarks/tools/mac-native-cargo-20261016.txt`

未做长期常驻、休眠唤醒或其他设备的 iCloud 同步验收；通知仅确认交给系统。原生应用使用 ad-hoc 签名，修改后重新构建可能需要再次授予系统权限。


### 日程工具追加验收

2026-10-16：两个系统权限已在更新后重新授权并核实。日程安排新增的两个工具、当日待办前缀过滤及完整验证结果见[日程安排](日程安排.md#当前验收记录2026-10-16)。当前常规测试1,116项通过，真实DeepSeek日程链路四轮43项检查通过，合成记录均已清理。

发送侧支持跨会话查找本机草稿、查询提交状态和替换附件，使用范围见[发送侧与Mac联动](邮件发送与Mac联动-20261016.md)。

可选自动代回复增加`mail_reply_pending`（只读建议/问题）和`mail_reply_prepare`（审批后转草稿，不发送），结合近期安排与画像，默认关闭。用法、收发提示词和边界见[邮件代回复](邮件代回复与双向提示词-20261016.md)。
