import AppKit
import Combine

/// 标准 macOS 设置窗口：左侧源列表分区，右侧详情。
/// 数据来自 `capabilities` 快照；只读展示，不绕过审批。
final class SettingsWindowController: NSWindowController, NSOutlineViewDataSource, NSOutlineViewDelegate {
    private let service: ChatService
    private var outlineView: NSOutlineView!
    private var detailRoot: NSView!
    private var detailScroll: NSScrollView!
    private var detailStack: NSStackView!
    private var snapshot: Capabilities?
    private var selected: Section = .overview
    private var cancellables = Set<AnyCancellable>()

    enum Section: Int, CaseIterable {
        case overview, tools, memory, tasks, ledger, system

        var title: String {
            switch self {
            case .overview: return "概览"
            case .tools: return "工具"
            case .memory: return "记忆"
            case .tasks: return "任务"
            case .ledger: return "台账"
            case .system: return "系统"
            }
        }

        var symbol: String {
            switch self {
            case .overview: return "info.circle"
            case .tools: return "wrench.and.screwdriver"
            case .memory: return "brain"
            case .tasks: return "clock"
            case .ledger: return "list.bullet.rectangle"
            case .system: return "gearshape"
            }
        }
    }

    init(service: ChatService) {
        self.service = service
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 820, height: 600),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "设置"
        window.minSize = NSSize(width: 700, height: 500)
        window.center()
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("YunXiSettings")
        super.init(window: window)
        build()

        service.$capabilities
            .receive(on: RunLoop.main)
            .sink { [weak self] capabilities in
                guard let self else { return }
                self.snapshot = capabilities
                self.rebuildDetail()
            }
            .store(in: &cancellables)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func showWindow(_ sender: Any?) {
        super.showWindow(sender)
        window?.makeKeyAndOrderFront(nil)
        service.requestCapabilities()
    }

    func show(section: Section) {
        select(section)
        showWindow(nil)
    }

    private func select(_ section: Section) {
        selected = section
        if let row = Section.allCases.firstIndex(of: section) {
            outlineView.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
        }
        rebuildDetail()
    }

    // MARK: - 布局

    private func build() {
        guard let content = window?.contentView else { return }

        let sidebar = NSVisualEffectView()
        sidebar.material = .sidebar
        sidebar.blendingMode = .behindWindow
        sidebar.state = .followsWindowActiveState
        sidebar.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(sidebar)

        let table = NSOutlineView()
        table.headerView = nil
        table.style = .sourceList
        table.selectionHighlightStyle = .regular
        table.rowHeight = 30
        table.dataSource = self
        table.delegate = self
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("section"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.translatesAutoresizingMaskIntoConstraints = false
        outlineView = table

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(scroll)

        let divider = NSBox()
        divider.boxType = .separator
        divider.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(divider)

        detailRoot = NSView()
        detailRoot.wantsLayer = true
        detailRoot.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        detailRoot.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(detailRoot)

        detailScroll = NSScrollView()
        detailScroll.hasVerticalScroller = true
        detailScroll.autohidesScrollers = true
        detailScroll.drawsBackground = false
        detailScroll.borderType = .noBorder
        detailScroll.translatesAutoresizingMaskIntoConstraints = false
        detailRoot.addSubview(detailScroll)

        let document = FlippedView()
        document.translatesAutoresizingMaskIntoConstraints = false
        detailScroll.documentView = document

        detailStack = NSStackView()
        detailStack.orientation = .vertical
        detailStack.alignment = .leading
        detailStack.spacing = 16
        detailStack.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(detailStack)

        NSLayoutConstraint.activate([
            sidebar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            sidebar.topAnchor.constraint(equalTo: content.topAnchor),
            sidebar.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            sidebar.widthAnchor.constraint(equalToConstant: 190),

            scroll.topAnchor.constraint(equalTo: sidebar.topAnchor, constant: 12),
            scroll.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 8),
            scroll.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -8),
            scroll.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor, constant: -8),

            divider.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            divider.topAnchor.constraint(equalTo: content.topAnchor),
            divider.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            divider.widthAnchor.constraint(equalToConstant: 1),

            detailRoot.leadingAnchor.constraint(equalTo: divider.trailingAnchor),
            detailRoot.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            detailRoot.topAnchor.constraint(equalTo: content.topAnchor),
            detailRoot.bottomAnchor.constraint(equalTo: content.bottomAnchor),

            detailScroll.topAnchor.constraint(equalTo: detailRoot.topAnchor),
            detailScroll.leadingAnchor.constraint(equalTo: detailRoot.leadingAnchor),
            detailScroll.trailingAnchor.constraint(equalTo: detailRoot.trailingAnchor),
            detailScroll.bottomAnchor.constraint(equalTo: detailRoot.bottomAnchor),

            document.widthAnchor.constraint(equalTo: detailScroll.contentView.widthAnchor),
            detailStack.topAnchor.constraint(equalTo: document.topAnchor, constant: 20),
            detailStack.leadingAnchor.constraint(equalTo: document.leadingAnchor, constant: 24),
            detailStack.trailingAnchor.constraint(equalTo: document.trailingAnchor, constant: -24),
            detailStack.bottomAnchor.constraint(equalTo: document.bottomAnchor, constant: -20),
        ])

        outlineView.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        rebuildDetail()
    }

    // MARK: - 侧栏

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        item == nil ? Section.allCases.count : 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        Section(rawValue: index)!
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool { false }

    func outlineView(_ outlineView: NSOutlineView, isGroupItem item: Any) -> Bool { false }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let section = item as? Section else { return nil }
        let identifier = NSUserInterfaceItemIdentifier("settings-section")
        let cell = (outlineView.makeView(withIdentifier: identifier, owner: self) as? NSTableCellView) ?? {
            let cell = NSTableCellView()
            let image = NSImageView()
            image.translatesAutoresizingMaskIntoConstraints = false
            let label = NSTextField(labelWithString: "")
            label.font = .systemFont(ofSize: 13)
            label.translatesAutoresizingMaskIntoConstraints = false
            cell.addSubview(image)
            cell.addSubview(label)
            cell.imageView = image
            cell.textField = label
            NSLayoutConstraint.activate([
                image.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 4),
                image.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                image.widthAnchor.constraint(equalToConstant: 16),
                image.heightAnchor.constraint(equalToConstant: 16),
                label.leadingAnchor.constraint(equalTo: image.trailingAnchor, constant: 8),
                label.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                label.trailingAnchor.constraint(lessThanOrEqualTo: cell.trailingAnchor, constant: -4),
            ])
            return cell
        }()
        cell.identifier = identifier
        cell.textField?.stringValue = section.title
        cell.imageView?.image = NSImage(systemSymbolName: section.symbol, accessibilityDescription: nil)
        cell.imageView?.contentTintColor = .secondaryLabelColor
        return cell
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        let row = outlineView.selectedRow
        guard row >= 0, let section = Section(rawValue: row) else { return }
        selected = section
        rebuildDetail()
    }

    // MARK: - 详情

    private func rebuildDetail() {
        guard detailStack != nil else { return }
        for view in detailStack.arrangedSubviews {
            detailStack.removeArrangedSubview(view)
            view.removeFromSuperview()
        }

        let header = NSStackView()
        header.orientation = .horizontal
        header.alignment = .centerY
        header.spacing = 8
        let title = NSTextField(labelWithString: selected.title)
        title.font = .systemFont(ofSize: 17, weight: .semibold)
        let spacer = NSView()
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)
        let refresh = NSButton(image: NSImage(systemSymbolName: "arrow.clockwise", accessibilityDescription: "刷新") ?? NSImage(), target: self, action: #selector(refresh))
        refresh.bezelStyle = .inline
        refresh.isBordered = false
        refresh.toolTip = "刷新"
        header.addArrangedSubview(title)
        header.addArrangedSubview(spacer)
        header.addArrangedSubview(refresh)
        detailStack.addArrangedSubview(header)
        header.widthAnchor.constraint(equalTo: detailStack.widthAnchor).isActive = true

        guard let snapshot else {
            detailStack.addArrangedSubview(hint("正在读取…"))
            return
        }

        switch selected {
        case .overview: buildOverview(snapshot)
        case .tools: buildTools(snapshot)
        case .memory: buildMemory(snapshot)
        case .tasks: buildTasks(snapshot)
        case .ledger: buildLedger(snapshot)
        case .system: buildSystem(snapshot)
        }
    }

    private func buildOverview(_ c: Capabilities) {
        let effort = ["auto": "自动", "on": "始终开", "off": "关"][c.bot.effort] ?? c.bot.effort
        addGroup("运行状态", rows: [
            row("模型", c.bot.model),
            row("思考", effort),
            row("当前会话", "\(c.bot.session) · \(c.bot.turns) 轮"),
            row("会话总数", "\(c.bot.sessionCount)"),
            row("最近会话", c.bot.latestSession.map { "\($0) · \(c.bot.latestTurns ?? 0) 轮" } ?? "（无）"),
        ])
        addGroup("身份资料", rows: [
            row("人格 persona.md", bytes(c.identity.personaBytes)),
            row("画像 profile.md", bytes(c.identity.profileBytes)),
            hint("身份修改只通过 identity_read / identity_update，先展示、逐次确认，再备份保存。"),
        ])
        addGroup("能力链路", rows: [
            row("对话", "在线 · \(c.bot.model)"),
            row("目标任务", "\(c.tasks.doTasks.count) 个（do）"),
            row("定时作业", "\(c.tasks.jobs.count) 个（add）"),
            row("主动陪伴", c.system.daemon.alive ? "值守运行中" : "值守未运行"),
            row("邮件巡览", c.system.mailConfigured ? "已配置" : "未配置"),
        ])
        addGroup("能力规模", rows: [
            row("工具", "\(c.tools.total) 个（MCP \(c.tools.mcpCount)）"),
            row("记忆", "\(c.memory.total) 条 · 待确认 \(c.memory.pendingTotal)"),
            row("知识文档", "\(c.memory.knowledgeDocuments) 篇"),
            row("台账", "\(c.ledger.events) 条事件 · \(bytes(c.ledger.bytes))"),
        ])
    }

    private func buildTools(_ c: Capabilities) {
        detailStack.addArrangedSubview(hint("共 \(c.tools.total) 个工具。只读默认放行；写入、执行、网络默认询问；不可逆永远人工。"))
        let groups = Dictionary(grouping: c.tools.items, by: \.capability)
        let order = ["只读", "写入", "执行", "网络", "不可逆", "未知"]
        for label in order + groups.keys.filter({ !order.contains($0) }).sorted() {
            guard let items = groups[label], !items.isEmpty else { continue }
            addGroup("\(label) · \(items.count)", rows: items.sorted { $0.name < $1.name }.map { toolRow($0) })
        }
        if !c.tools.mcpProblems.isEmpty {
            addGroup("MCP 问题", rows: c.tools.mcpProblems.map { hint($0) })
        }
    }

    private func buildMemory(_ c: Capabilities) {
        addGroup("记忆", rows: [
            row("已确认", "\(c.memory.total) 条"),
            row("待确认候选", "\(c.memory.pendingTotal) 条"),
            row("待跟进", "\(c.memory.followups) 条"),
            row("知识文档", "\(c.memory.knowledgeDocuments) 篇"),
        ])
        if !c.memory.pending.isEmpty {
            addGroup("待确认", rows: c.memory.pending.map { item in
                multiline("\(item.kind) · \(item.text)\n来源 #\(item.sourceSeq)：「\(item.quote)」")
            })
        }
        if c.memory.recent.isEmpty {
            addGroup("最近记忆", rows: [hint("还没有已确认的长期记忆。")])
        } else {
            addGroup("最近记忆", rows: c.memory.recent.map { multiline("\($0.kind) · \($0.text)") })
        }
    }

    private func buildTasks(_ c: Capabilities) {
        if c.tasks.jobs.isEmpty {
            addGroup("定时作业", rows: [hint("还没有定时作业。")])
        } else {
            addGroup("定时作业 · \(c.tasks.jobs.count)", rows: c.tasks.jobs.map { job in
                multiline("\(job.name) · \(job.state)\n\(job.trigger) · \(job.command)\(job.lastError.map { "\n上次错误：\($0)" } ?? "")")
            })
        }
        if c.tasks.doTasks.isEmpty {
            addGroup("目标任务", rows: [hint("还没有用 do 提交过任务。")])
        } else {
            addGroup("目标任务 · \(c.tasks.doTasks.count)", rows: c.tasks.doTasks.map { task in
                multiline("\(task.goal) · \(task.state)\n\(task.id) · 步骤 \(task.done)/\(task.steps) · 失败 \(task.failed)")
            })
        }
    }

    private func buildLedger(_ c: Capabilities) {
        let rate = c.cost.cacheHitRate.map { String(format: "%.1f%%", $0 * 100) } ?? "未知"
        addGroup("模型调用", rows: [
            row("调用次数", "\(c.cost.calls)（思考 \(c.cost.thinkingCalls)）"),
            row("Token", "入 \(grouped(c.cost.promptTokens)) / 出 \(grouped(c.cost.completionTokens))"),
            row("缓存命中", "\(rate) · 命中 \(grouped(c.cost.cacheHitTokens))"),
            row("累计花费", String(format: "¥%.4f（缓存省 ¥%.4f）", c.cost.total, c.cost.saved)),
        ])
        addGroup("事件类型 Top 12", rows: c.ledger.topKinds.map { row($0.kind, "\($0.count)") })
        addGroup("最近事件", rows: c.ledger.recent.map { event in
            row("#\(event.seq) \(event.kind)", Self.dateFormatter.string(from: Date(timeIntervalSince1970: event.at / 1000)))
        })
    }

    private func buildSystem(_ c: Capabilities) {
        addGroup("常驻与自启", rows: [
            row("值守进程", (c.system.daemon.alive ? "运行中" : "未运行") + (c.system.daemon.pid.map { " · pid \($0)" } ?? "")),
            row("登录自启", c.system.autostart.registered ? "已注册" : "未注册"),
        ])
        if let companion = c.system.companion {
            addGroup("本机陪伴", rows: [
                row("本机模型", companion.localModelState),
                row("上次记忆整理", timestamp(companion.lastMemoryAt)),
                row("上次知识入库", timestamp(companion.lastKnowledgeAt)),
                row("待确认记忆", "\(companion.pendingMemories) 条"),
            ])
        }
        addGroup("本地服务", rows: c.system.services.map { service in
            row(service.name, service.listening ? "已监听 : \(service.port)" : "未启动（按需）")
        })
        addGroup("运行环境", rows: [
            row("平台", c.platform),
            row("数据目录", c.home),
            row("工作上下文", c.home + "/work"),
            row("邮件巡览", c.system.mailConfigured ? "已配置" : "未配置"),
            row("本地模型", c.system.models.isEmpty ? "（无）" : c.system.models.joined(separator: " · ")),
        ])
        let buttons = NSStackView(views: [
            button("打开数据目录", #selector(openDataFolder)),
            button("显示台账", #selector(revealLedger)),
            button("显示人格", #selector(revealPersona)),
            button("打开工作上下文", #selector(openWorkFolder)),
            button("显示工作人格", #selector(revealWorkPersona)),
        ])
        buttons.orientation = .horizontal
        buttons.spacing = 8
        detailStack.addArrangedSubview(buttons)
    }

    // MARK: - 组件

    private func addGroup(_ title: String, rows: [NSView]) {
        let header = NSTextField(labelWithString: title)
        header.font = .systemFont(ofSize: 11, weight: .semibold)
        header.textColor = .secondaryLabelColor
        let stack = NSStackView(views: rows)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 6
        let box = NSStackView(views: [header, stack])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 5
        detailStack.addArrangedSubview(box)
        box.widthAnchor.constraint(equalTo: detailStack.widthAnchor).isActive = true
        for row in rows {
            row.widthAnchor.constraint(lessThanOrEqualTo: box.widthAnchor).isActive = true
        }
    }

    private func row(_ key: String, _ value: String) -> NSView {
        let keyLabel = NSTextField(labelWithString: key)
        keyLabel.font = .systemFont(ofSize: 11)
        keyLabel.textColor = .secondaryLabelColor
        keyLabel.setContentHuggingPriority(.required, for: .horizontal)
        keyLabel.widthAnchor.constraint(greaterThanOrEqualToConstant: 112).isActive = true
        let valueLabel = NSTextField(wrappingLabelWithString: value)
        valueLabel.font = .systemFont(ofSize: 13)
        valueLabel.preferredMaxLayoutWidth = 500
        let stack = NSStackView(views: [keyLabel, valueLabel])
        stack.orientation = .horizontal
        stack.alignment = .top
        stack.spacing = 10
        return stack
    }

    private func multiline(_ text: String) -> NSTextField {
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = .systemFont(ofSize: 12)
        label.textColor = .labelColor
        label.preferredMaxLayoutWidth = 520
        return label
    }

    private func hint(_ text: String) -> NSTextField {
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = .systemFont(ofSize: 11)
        label.textColor = .secondaryLabelColor
        label.preferredMaxLayoutWidth = 520
        return label
    }

    private func toolRow(_ tool: ToolItem) -> NSView {
        let name = NSTextField(labelWithString: tool.name)
        name.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        let meta = NSTextField(labelWithString: "\(tool.capability) · \(tool.approval == "auto" ? "免问" : "询问")")
        meta.font = .systemFont(ofSize: 10.5)
        meta.textColor = .secondaryLabelColor
        let top = NSStackView(views: [name, meta])
        top.orientation = .horizontal
        top.spacing = 8
        let desc = hint(tool.description)
        let stack = NSStackView(views: [top, desc])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 2
        return stack
    }

    private func button(_ title: String, _ action: Selector) -> NSButton {
        let button = NSButton(title: title, target: self, action: action)
        button.bezelStyle = .rounded
        button.controlSize = .small
        return button
    }

    private func timestamp(_ millis: Double?) -> String {
        guard let millis else { return "还没有" }
        return Self.dateFormatter.string(from: Date(timeIntervalSince1970: millis / 1000))
    }

    private func bytes(_ value: Int) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(value), countStyle: .file)
    }

    private func grouped(_ value: Int) -> String {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f.string(from: NSNumber(value: value)) ?? "\(value)"
    }

    private static let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "MM-dd HH:mm"
        return f
    }()

    @objc private func refresh() {
        service.requestCapabilities()
    }

    @objc private func openDataFolder() {
        if let home = snapshot?.home { NSWorkspace.shared.open(URL(fileURLWithPath: home)) }
    }

    @objc private func openWorkFolder() {
        if let home = snapshot?.home {
            let work = home + "/work"
            try? FileManager.default.createDirectory(atPath: work, withIntermediateDirectories: true)
            NSWorkspace.shared.open(URL(fileURLWithPath: work))
        }
    }

    @objc private func revealWorkPersona() {
        if let home = snapshot?.home {
            let work = home + "/work"
            let persona = work + "/persona.md"
            try? FileManager.default.createDirectory(atPath: work, withIntermediateDirectories: true)
            if FileManager.default.fileExists(atPath: persona) {
                NSWorkspace.shared.selectFile(persona, inFileViewerRootedAtPath: work)
            } else {
                NSWorkspace.shared.open(URL(fileURLWithPath: work))
            }
        }
    }

    @objc private func revealLedger() {
        guard let path = snapshot?.system.ledgerPath else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: path))
    }

    @objc private func revealPersona() {
        guard let home = snapshot?.home else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: home + "/persona.md")])
    }
}
