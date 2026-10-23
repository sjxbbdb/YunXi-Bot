import AppKit
import Combine

/// “云熙的能力”窗口：把 chat-server 的能力快照按类别摊开。
/// 只读展示——不提供绕过审批的开关，操作仍走对话与审批链路。
final class CapabilityWindowController: NSWindowController {
    private let service: ChatService
    private var segmented: NSSegmentedControl!
    private var stack: NSStackView!
    private var selectedIndex = 0
    private var subtitle = NSTextField(labelWithString: "正在读取后端能力…")
    private var footer = NSTextField(labelWithString: "")
    private var snapshot: Capabilities?
    private var cancellables = Set<AnyCancellable>()

    private static let capabilityOrder = ["只读", "写入", "执行", "网络", "不可逆", "未知"]
    private static let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "MM-dd HH:mm"
        return f
    }()

    init(service: ChatService) {
        self.service = service
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 840, height: 660),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙的能力"
        window.minSize = NSSize(width: 720, height: 540)
        window.isReleasedWhenClosed = false
        window.center()
        super.init(window: window)
        build()

        service.$capabilities
            .receive(on: RunLoop.main)
            .sink { [weak self] capabilities in
                guard let capabilities else { return }
                self?.apply(capabilities)
            }
            .store(in: &cancellables)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func showWindow(_ sender: Any?) {
        super.showWindow(sender)
        window?.makeKeyAndOrderFront(nil)
        service.requestCapabilities()
        // 演示/验收入口：YUNXI_CAP_TAB=0…5 指定打开的分类。
        if let raw = ProcessInfo.processInfo.environment["YUNXI_CAP_TAB"], let index = Int(raw), (0...5).contains(index) {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak self] in
                guard let self else { return }
                self.selectedIndex = index
                self.segmented.selectedSegment = index
                self.rebuildSelected()
            }
        }
    }

    // MARK: - 布局

    private func build() {
        guard let content = window?.contentView else { return }
        let background = NSVisualEffectView()
        background.material = .underWindowBackground
        background.blendingMode = .behindWindow
        background.state = .active
        background.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(background)

        let title = NSTextField(labelWithString: "云熙的能力")
        title.font = .systemFont(ofSize: 20, weight: .semibold)
        subtitle.font = .systemFont(ofSize: 11)
        subtitle.textColor = .secondaryLabelColor
        subtitle.lineBreakMode = .byTruncatingMiddle

        let titleStack = NSStackView(views: [title, subtitle])
        titleStack.orientation = .vertical
        titleStack.alignment = .leading
        titleStack.spacing = 3

        let spacer = NSView()
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)

        let refresh = NSButton(title: "刷新", target: self, action: #selector(refresh))
        refresh.bezelStyle = .rounded
        refresh.controlSize = .small
        let openData = NSButton(title: "打开数据目录", target: self, action: #selector(openDataFolder))
        openData.bezelStyle = .rounded
        openData.controlSize = .small

        let header = NSStackView(views: [titleStack, spacer, refresh, openData])
        header.orientation = .horizontal
        header.alignment = .centerY
        header.spacing = 8
        header.translatesAutoresizingMaskIntoConstraints = false

        segmented = NSSegmentedControl(
            labels: ["概览", "工具", "记忆", "任务", "台账", "系统"],
            trackingMode: .selectOne,
            target: self,
            action: #selector(segmentChanged)
        )
        segmented.selectedSegment = 0
        segmented.controlSize = .small
        segmented.translatesAutoresizingMaskIntoConstraints = false

        let (scroll, contentStack) = makeScrollableStack()
        stack = contentStack

        footer.font = .systemFont(ofSize: 9.5)
        footer.textColor = .tertiaryLabelColor
        footer.translatesAutoresizingMaskIntoConstraints = false

        background.addSubview(header)
        background.addSubview(segmented)
        background.addSubview(scroll)
        background.addSubview(footer)

        NSLayoutConstraint.activate([
            background.topAnchor.constraint(equalTo: content.topAnchor),
            background.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            background.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            background.bottomAnchor.constraint(equalTo: content.bottomAnchor),

            header.topAnchor.constraint(equalTo: background.topAnchor, constant: 14),
            header.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 16),
            header.trailingAnchor.constraint(equalTo: background.trailingAnchor, constant: -16),

            segmented.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 10),
            segmented.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 16),
            segmented.trailingAnchor.constraint(lessThanOrEqualTo: background.trailingAnchor, constant: -16),

            scroll.topAnchor.constraint(equalTo: segmented.bottomAnchor, constant: 8),
            scroll.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 12),
            scroll.trailingAnchor.constraint(equalTo: background.trailingAnchor, constant: -12),
            scroll.bottomAnchor.constraint(equalTo: footer.topAnchor, constant: -6),

            footer.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 18),
            footer.trailingAnchor.constraint(lessThanOrEqualTo: background.trailingAnchor, constant: -18),
            footer.bottomAnchor.constraint(equalTo: background.bottomAnchor, constant: -8),
        ])

        rebuildEmptyState()
    }

    private func makeScrollableStack() -> (NSScrollView, NSStackView) {
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false

        let document = FlippedView()
        document.translatesAutoresizingMaskIntoConstraints = false
        scroll.documentView = document

        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsetsZero
        stack.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(stack)

        NSLayoutConstraint.activate([
            document.widthAnchor.constraint(equalTo: scroll.contentView.widthAnchor),
            stack.topAnchor.constraint(equalTo: document.topAnchor),
            stack.leadingAnchor.constraint(equalTo: document.leadingAnchor, constant: 12),
            stack.trailingAnchor.constraint(equalTo: document.trailingAnchor, constant: -12),
            stack.bottomAnchor.constraint(equalTo: document.bottomAnchor),
        ])
        return (scroll, stack)
    }

    private func rebuildEmptyState() {
        clear(stack)
        stack.addArrangedSubview(hint("正在读取后端能力…"))
        fitStacks()
    }

    // MARK: - 应用快照

    private func apply(_ capabilities: Capabilities) {
        snapshot = capabilities
        subtitle.stringValue = "v\(capabilities.version) · \(capabilities.home)"
        footer.stringValue = "生成于 \(Self.dateFormatter.string(from: Date(timeIntervalSince1970: capabilities.generatedAt / 1000))) · \(capabilities.tools.total) 个工具 · \(capabilities.ledger.events) 条台账事件 · 只读面板，不绕过审批"
        segmented.setLabel("工具 \(capabilities.tools.total)", forSegment: 1)
        segmented.setLabel("记忆 \(capabilities.memory.total)", forSegment: 2)
        segmented.setLabel("任务 \(capabilities.tasks.jobs.count + capabilities.tasks.doTasks.count)", forSegment: 3)
        rebuildSelected()
    }

    private func rebuildSelected() {
        clear(stack)
        guard let snapshot else {
            stack.addArrangedSubview(hint("正在读取后端能力…"))
            fitStacks()
            return
        }
        switch selectedIndex {
        case 0: rebuildOverview(snapshot)
        case 1: rebuildTools(snapshot)
        case 2: rebuildMemory(snapshot)
        case 3: rebuildTasks(snapshot)
        case 4: rebuildLedger(snapshot)
        default: rebuildSystem(snapshot)
        }
        fitStacks()
    }

    @objc private func segmentChanged() {
        selectedIndex = segmented.selectedSegment
        rebuildSelected()
    }

    /// 让每张卡片/每一行跟内容区同宽；否则 NSStackView 会按内在宽度把长文本挤成一条。
    private func fitStacks() {
        guard let stack else { return }
        for view in stack.arrangedSubviews {
            view.translatesAutoresizingMaskIntoConstraints = false
            view.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        }
    }

    private func rebuildOverview(_ c: Capabilities) {
        clear(stack)
        stack.addArrangedSubview(hint("陪伴型常驻 Agent · 对话、记忆、任务、工具审批共用同一条台账与权限链。"))

        let metrics = NSStackView(views: [
            metric("\(c.tools.total)", "工具"),
            metric("\(c.memory.total)", "记忆"),
            metric("\(c.tasks.jobs.count)", "定时任务"),
            metric("\(c.tasks.doTasks.count)", "目标任务"),
            metric("\(c.ledger.events)", "台账事件"),
            metric("\(c.memory.knowledgeDocuments)", "知识文档"),
        ])
        metrics.orientation = .horizontal
        metrics.alignment = .top
        metrics.spacing = 24
        metrics.edgeInsets = NSEdgeInsets(top: 4, left: 2, bottom: 4, right: 2)
        stack.addArrangedSubview(card([metrics]))

        stack.addArrangedSubview(section("运行状态"))
        let effort = ["auto": "自动", "on": "始终开", "off": "关"][c.bot.effort] ?? c.bot.effort
        stack.addArrangedSubview(card([
            row("模型", c.bot.model),
            row("思考", effort),
            row("当前会话", "\(c.bot.session) · \(c.bot.turns) 轮"),
            row("会话总数", "\(c.bot.sessionCount)"),
            row("最近会话", c.bot.latestSession.map { "\($0) · \(c.bot.latestTurns ?? 0) 轮" } ?? "（无）"),
        ]))

        stack.addArrangedSubview(section("身份资料"))
        let persona = ByteCountFormatter.string(fromByteCount: Int64(c.identity.personaBytes), countStyle: .file)
        let profile = ByteCountFormatter.string(fromByteCount: Int64(c.identity.profileBytes), countStyle: .file)
        stack.addArrangedSubview(card([
            row("人格 persona.md", persona),
            row("使用者画像 profile.md", profile),
            hint("身份修改只通过 identity_read / identity_update，先展示、逐次确认，再备份保存。"),
        ]))

        stack.addArrangedSubview(section("能力链路"))
        stack.addArrangedSubview(card([
            row("对话", "在线 · \(c.bot.model)"),
            row("目标任务", "\(c.tasks.doTasks.count) 个（do）"),
            row("定时作业", "\(c.tasks.jobs.count) 个（add）"),
            row("主动陪伴", c.system.daemon.alive ? "值守运行中" : "值守未运行"),
            row("邮件巡览", c.system.mailConfigured ? "已配置" : "未配置"),
        ]))

        stack.addArrangedSubview(section("能力分组"))
        let caps = c.tools.byCapability.map { "\($0.label) \($0.count)" }.joined(separator: " · ")
        stack.addArrangedSubview(card([
            hint(caps),
            hint("只读默认放行；写入、执行、网络默认询问；不可逆永远人工，且“总是允许”无效。"),
        ]))
    }

    private func rebuildTools(_ c: Capabilities) {
        clear(stack)
        stack.addArrangedSubview(hint("共 \(c.tools.total) 个工具（含 MCP \(c.tools.mcpCount) 个）。默认免问只适用于只读；具体执行仍受规则与审批约束。"))
        if !c.tools.mcpProblems.isEmpty {
            stack.addArrangedSubview(card([hint("MCP 问题：\n" + c.tools.mcpProblems.joined(separator: "\n"))]))
        }

        let groups = Dictionary(grouping: c.tools.items, by: \.capability)
        let orderedLabels = Self.capabilityOrder + groups.keys.filter { !Self.capabilityOrder.contains($0) }.sorted()
        for label in orderedLabels {
            guard let items = groups[label], !items.isEmpty else { continue }
            stack.addArrangedSubview(section("\(label) · \(items.count)"))
            for tool in items.sorted(by: { $0.name < $1.name }) {
                stack.addArrangedSubview(toolRow(tool))
            }
        }
    }

    private func rebuildMemory(_ c: Capabilities) {
        clear(stack)
        let kinds = c.memory.byKind.map { "\($0.label) \($0.count)" }.joined(separator: " · ")
        stack.addArrangedSubview(hint("长期记忆 \(c.memory.total) 条" + (kinds.isEmpty ? "" : "：\(kinds)") + "。记忆由台账投影，可忘但有记录；画像候选需人工接受。"))
        stack.addArrangedSubview(card([
            row("待确认候选", "\(c.memory.pendingTotal)"),
            row("待跟进事项", "\(c.memory.followups)"),
            row("知识文档", "\(c.memory.knowledgeDocuments)"),
        ]))

        if !c.memory.pending.isEmpty {
            stack.addArrangedSubview(section("待确认的记忆候选"))
            for item in c.memory.pending {
                stack.addArrangedSubview(card([
                    badge(item.kind, color: .systemOrange),
                    paragraph(item.text, mono: false),
                    hint("来源 #\(item.sourceSeq)：「\(item.quote)」"),
                    hint("在终端用 yunxi-bot memory --accept \(item.id) 确认，或 --reject \(item.id) 拒绝。"),
                ]))
            }
        }

        if c.memory.recent.isEmpty {
            stack.addArrangedSubview(card([hint("还没有已确认的长期记忆。可用 yunxi-bot remember 或在对话中让她记住。")]))
        } else {
            stack.addArrangedSubview(section("最近记忆"))
            for item in c.memory.recent {
                stack.addArrangedSubview(card([
                    NSStackView(views: [badge(item.kind, color: .systemTeal), hint("权重 \(item.weight) · \(Self.dateFormatter.string(from: Date(timeIntervalSince1970: item.createdAt / 1000)))")]),
                    paragraph(item.text),
                ]))
            }
        }
    }

    private func rebuildTasks(_ c: Capabilities) {
        clear(stack)
        stack.addArrangedSubview(hint("定时作业直接执行 argv，不经过生成模型；目标任务（do）由模型拆解、逐步执行，均写入台账。"))

        stack.addArrangedSubview(section("定时作业 · \(c.tasks.jobs.count)"))
        if c.tasks.jobs.isEmpty {
            stack.addArrangedSubview(card([hint("还没有定时作业。终端 yunxi-bot add <名称> --every 3600 -- <命令> 可以创建。")]))
        } else {
            for job in c.tasks.jobs {
                stack.addArrangedSubview(card([
                    NSStackView(views: [paragraph(job.name, mono: true), badge(job.state, color: job.state.contains("失败") || job.state.contains("停用") ? .systemRed : .systemGreen)]),
                    hint("\(job.trigger) · \(job.irreversible ? "不可逆（逐次批准）" : "常规")"),
                    hint(job.command, mono: true),
                    job.lastError.map { hint("上次错误：\($0)") },
                ].compactMap { $0 }))
            }
        }

        stack.addArrangedSubview(section("目标任务 · \(c.tasks.doTasks.count)"))
        if c.tasks.doTasks.isEmpty {
            stack.addArrangedSubview(card([hint("还没有用 do 提交过任务。")]))
        } else {
            for task in c.tasks.doTasks {
                stack.addArrangedSubview(card([
                    NSStackView(views: [paragraph(task.goal, mono: false), badge(task.state, color: .systemBlue)]),
                    hint("\(task.id) · 步骤 \(task.done)/\(task.steps) 完成 · 失败 \(task.failed) · 跳过 \(task.skipped)"),
                ]))
            }
        }
    }

    private func rebuildLedger(_ c: Capabilities) {
        clear(stack)
        let rate = c.cost.cacheHitRate.map { String(format: "%.1f%%", $0 * 100) } ?? "未知"
        stack.addArrangedSubview(hint("台账只追加、可重建状态；以下是从 \(c.ledger.events) 条事件实时投影。"))
        stack.addArrangedSubview(card([
            row("模型调用", "\(c.cost.calls) 次（思考 \(c.cost.thinkingCalls)）"),
            row("Token", "入 \(Self.grouped(c.cost.promptTokens)) / 出 \(Self.grouped(c.cost.completionTokens))"),
            row("缓存命中", "\(rate) · 命中 \(Self.grouped(c.cost.cacheHitTokens))"),
            row("累计花费", String(format: "¥%.4f（缓存省 ¥%.4f）", c.cost.total, c.cost.saved)),
            row("台账体积", ByteCountFormatter.string(fromByteCount: Int64(c.ledger.bytes), countStyle: .file)),
        ]))

        stack.addArrangedSubview(section("事件类型 Top 12"))
        for item in c.ledger.topKinds {
            stack.addArrangedSubview(kvRow(item.kind, "\(item.count)"))
        }

        stack.addArrangedSubview(section("最近事件"))
        for event in c.ledger.recent {
            let time = Self.dateFormatter.string(from: Date(timeIntervalSince1970: event.at / 1000))
            stack.addArrangedSubview(kvRow("#\(event.seq) \(event.kind)", time))
        }
        stack.addArrangedSubview(hint("完整台账：\(c.system.ledgerPath)"))
    }

    private func rebuildSystem(_ c: Capabilities) {
        clear(stack)

        stack.addArrangedSubview(section("常驻与自启"))
        let daemonState = c.system.daemon.alive ? "运行中" : (c.system.daemon.lockPresent ? "锁存在，进程未运行" : "未运行")
        var daemonRows: [NSView] = [
            row("值守进程", daemonState + (c.system.daemon.pid.map { " · pid \($0)" } ?? "")),
            row("登录自启", c.system.autostart.registered ? "已注册" : "未注册"),
        ]
        if let path = c.system.autostart.path {
            daemonRows.append(hint(path, mono: true))
        }
        stack.addArrangedSubview(card(daemonRows))

        stack.addArrangedSubview(section("本地服务"))
        for service in c.system.services {
            stack.addArrangedSubview(kvRow(
                "\(service.listening ? "●" : "○") \(service.name) : \(service.port)",
                service.listening ? "已监听" : "未启动（按需）"
            ))
        }

        stack.addArrangedSubview(section("运行环境"))
        stack.addArrangedSubview(card([
            row("平台", c.platform),
            row("数据目录", c.home),
            row("邮件巡检", c.system.mailConfigured ? "已配置" : "未配置"),
            row("可回显会话", "\(c.system.chatUiSessions)"),
            row("本地模型", c.system.models.isEmpty ? "（无）" : c.system.models.joined(separator: " · ")),
        ]))

        stack.addArrangedSubview(section("打开"))
        let buttons = NSStackView(views: [
            smallButton("数据目录", #selector(openDataFolder)),
            smallButton("人格文件", #selector(revealPersona)),
            smallButton("画像文件", #selector(revealProfile)),
            smallButton("台账", #selector(revealLedger)),
        ])
        buttons.orientation = .horizontal
        buttons.spacing = 8
        stack.addArrangedSubview(buttons)
        stack.addArrangedSubview(hint("面板只读：修改人格/画像走 identity_update，执行动作走对话审批；不会从这里绕过权限。"))
    }

    // MARK: - 组件

    private func clear(_ stack: NSStackView) {
        for view in stack.arrangedSubviews {
            stack.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
    }

    private func card(_ children: [NSView]) -> NSView {
        let box = NSView()
        box.wantsLayer = true
        box.layer?.cornerRadius = 12
        box.layer?.backgroundColor = NSColor.adaptive(
            light: NSColor(calibratedWhite: 0.985, alpha: 1),
            dark: NSColor(calibratedWhite: 0.16, alpha: 1)
        ).cgColor
        box.layer?.borderWidth = 1
        box.layer?.borderColor = NSColor.adaptive(
            light: NSColor(calibratedWhite: 0.88, alpha: 1),
            dark: NSColor(calibratedWhite: 0.28, alpha: 1)
        ).cgColor
        box.translatesAutoresizingMaskIntoConstraints = false

        let stack = NSStackView(views: children)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        box.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: box.topAnchor, constant: 10),
            stack.bottomAnchor.constraint(equalTo: box.bottomAnchor, constant: -10),
            stack.leadingAnchor.constraint(equalTo: box.leadingAnchor, constant: 12),
            stack.trailingAnchor.constraint(equalTo: box.trailingAnchor, constant: -12),
        ])
        return box
    }

    private func section(_ text: String) -> NSTextField {
        let label = NSTextField(labelWithString: text)
        label.font = .systemFont(ofSize: 11, weight: .semibold)
        label.textColor = .secondaryLabelColor
        return label
    }

    private func hint(_ text: String, mono: Bool = false) -> NSTextField {
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = mono ? .monospacedSystemFont(ofSize: 10, weight: .regular) : .systemFont(ofSize: 11)
        label.textColor = .secondaryLabelColor
        label.preferredMaxLayoutWidth = 700
        label.maximumNumberOfLines = 0
        return label
    }

    private func paragraph(_ text: String, mono: Bool = false) -> NSTextField {
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = mono ? .monospacedSystemFont(ofSize: 11, weight: .regular) : .systemFont(ofSize: 12)
        label.textColor = .labelColor
        label.preferredMaxLayoutWidth = 700
        label.maximumNumberOfLines = 0
        return label
    }

    private func row(_ key: String, _ value: String) -> NSStackView {
        let keyLabel = NSTextField(labelWithString: key)
        keyLabel.font = .systemFont(ofSize: 11)
        keyLabel.textColor = .secondaryLabelColor
        keyLabel.setContentHuggingPriority(.required, for: .horizontal)
        keyLabel.widthAnchor.constraint(greaterThanOrEqualToConstant: 96).isActive = true
        let valueLabel = NSTextField(wrappingLabelWithString: value)
        valueLabel.font = .systemFont(ofSize: 11)
        valueLabel.preferredMaxLayoutWidth = 560
        let stack = NSStackView(views: [keyLabel, valueLabel])
        stack.orientation = .horizontal
        stack.alignment = .top
        stack.spacing = 8
        return stack
    }

    private func kvRow(_ key: String, _ value: String) -> NSView {
        let keyLabel = NSTextField(labelWithString: key)
        keyLabel.font = .monospacedSystemFont(ofSize: 10.5, weight: .regular)
        keyLabel.textColor = .secondaryLabelColor
        keyLabel.lineBreakMode = .byTruncatingTail
        keyLabel.setContentHuggingPriority(.defaultLow, for: .horizontal)
        let valueLabel = NSTextField(labelWithString: value)
        valueLabel.font = .systemFont(ofSize: 10.5)
        valueLabel.textColor = .tertiaryLabelColor
        valueLabel.setContentHuggingPriority(.required, for: .horizontal)
        let stack = NSStackView(views: [keyLabel, valueLabel])
        stack.orientation = .horizontal
        stack.spacing = 8
        return stack
    }

    private func metric(_ value: String, _ label: String) -> NSView {
        let valueLabel = NSTextField(labelWithString: value)
        valueLabel.font = .systemFont(ofSize: 20, weight: .semibold)
        let name = NSTextField(labelWithString: label)
        name.font = .systemFont(ofSize: 10)
        name.textColor = .secondaryLabelColor
        let stack = NSStackView(views: [valueLabel, name])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 1
        return stack
    }

    private func badge(_ text: String, color: NSColor) -> NSView {
        let label = NSTextField(labelWithString: text)
        label.font = .systemFont(ofSize: 9, weight: .semibold)
        label.textColor = color
        let box = NSView()
        box.wantsLayer = true
        box.layer?.cornerRadius = 6
        box.layer?.backgroundColor = color.withAlphaComponent(0.12).cgColor
        label.translatesAutoresizingMaskIntoConstraints = false
        box.addSubview(label)
        NSLayoutConstraint.activate([
            label.topAnchor.constraint(equalTo: box.topAnchor, constant: 2),
            label.bottomAnchor.constraint(equalTo: box.bottomAnchor, constant: -2),
            label.leadingAnchor.constraint(equalTo: box.leadingAnchor, constant: 6),
            label.trailingAnchor.constraint(equalTo: box.trailingAnchor, constant: -6),
        ])
        box.translatesAutoresizingMaskIntoConstraints = false
        return box
    }

    private func toolRow(_ tool: ToolItem) -> NSView {
        let color: NSColor
        switch tool.capability {
        case "只读": color = .systemGreen
        case "写入": color = .systemOrange
        case "执行": color = .systemRed
        case "网络": color = .systemBlue
        case "不可逆": color = .systemPurple
        default: color = .systemGray
        }
        let approval = tool.approval == "auto" ? "免问" : "默认询问"
        let head = NSStackView(views: [
            paragraph(tool.name, mono: true),
            badge(tool.capability, color: color),
            badge(approval, color: tool.approval == "auto" ? .systemGreen : .systemOrange),
            tool.privateIo ? badge("私有参数", color: .systemGray) : NSView(),
        ])
        head.orientation = .horizontal
        head.spacing = 6
        head.alignment = .centerY
        let desc = hint(tool.description)

        let box = NSView()
        box.wantsLayer = true
        box.layer?.cornerRadius = 8
        box.layer?.backgroundColor = NSColor.adaptive(
            light: NSColor(calibratedWhite: 0.975, alpha: 1),
            dark: NSColor(calibratedWhite: 0.185, alpha: 1)
        ).cgColor
        let stack = NSStackView(views: [head, desc])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 3
        stack.translatesAutoresizingMaskIntoConstraints = false
        box.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: box.topAnchor, constant: 7),
            stack.bottomAnchor.constraint(equalTo: box.bottomAnchor, constant: -7),
            stack.leadingAnchor.constraint(equalTo: box.leadingAnchor, constant: 10),
            stack.trailingAnchor.constraint(equalTo: box.trailingAnchor, constant: -10),
        ])
        return box
    }

    private func smallButton(_ title: String, _ action: Selector) -> NSButton {
        let button = NSButton(title: title, target: self, action: action)
        button.bezelStyle = .rounded
        button.controlSize = .small
        return button
    }

    private static func grouped(_ number: Int) -> String {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f.string(from: NSNumber(value: number)) ?? "\(number)"
    }

    // MARK: - 动作

    @objc private func refresh() {
        service.requestCapabilities()
    }

    private func open(_ path: String) {
        NSWorkspace.shared.open(URL(fileURLWithPath: path))
    }

    @objc private func openDataFolder() {
        if let home = snapshot?.home { open(home) }
    }

    @objc private func revealPersona() {
        if let home = snapshot?.home {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: home + "/persona.md")])
        }
    }

    @objc private func revealProfile() {
        if let home = snapshot?.home {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: home + "/profile.md")])
        }
    }

    @objc private func revealLedger() {
        guard let path = snapshot?.system.ledgerPath else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }
}
