import AppKit
import Combine

// MARK: - 共用色与视图

extension NSColor {
    static var yunxiAccent: NSColor { .systemPink }

    static func adaptive(light: NSColor, dark: NSColor) -> NSColor {
        NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
        }
    }
}

final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

final class AvatarView: NSView {
    init(diameter: CGFloat, fontSize: CGFloat) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = diameter / 2
        layer?.cornerCurve = .continuous
        layer?.backgroundColor = NSColor.systemPink.cgColor
        let label = NSTextField(labelWithString: "熙")
        label.font = .systemFont(ofSize: fontSize, weight: .semibold)
        label.textColor = .white
        label.alignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)
        NSLayoutConstraint.activate([
            widthAnchor.constraint(equalToConstant: diameter),
            heightAnchor.constraint(equalToConstant: diameter),
            label.centerXAnchor.constraint(equalTo: centerXAnchor),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        translatesAutoresizingMaskIntoConstraints = false
    }

    required init?(coder: NSCoder) { fatalError() }
}

private func symbol(_ name: String, size: CGFloat, weight: NSFont.Weight = .regular) -> NSImage? {
    let config = NSImage.SymbolConfiguration(pointSize: size, weight: weight)
    return NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(config)
}

// MARK: - 消息行

final class MessageRowView: NSView {
    private let role: ChatMessage.Role
    private let bubble: NSView?
    private let label: NSTextField?

    init(message: ChatMessage) {
        role = message.role
        if message.role == .system {
            bubble = nil
            label = NSTextField(wrappingLabelWithString: message.text)
        } else {
            let box = NSView()
            box.wantsLayer = true
            box.layer?.cornerRadius = 14
            box.layer?.cornerCurve = .continuous
            box.translatesAutoresizingMaskIntoConstraints = false
            bubble = box
            label = NSTextField(wrappingLabelWithString: "")
        }
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false

        if role == .system {
            guard let label else { return }
            label.font = .systemFont(ofSize: 11)
            label.textColor = .tertiaryLabelColor
            label.alignment = .center
            label.translatesAutoresizingMaskIntoConstraints = false
            addSubview(label)
            NSLayoutConstraint.activate([
                label.topAnchor.constraint(equalTo: topAnchor, constant: 2),
                label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
                label.centerXAnchor.constraint(equalTo: centerXAnchor),
                label.widthAnchor.constraint(lessThanOrEqualToConstant: 520),
            ])
            return
        }

        guard let bubble, let label else { return }
        label.isSelectable = true
        label.maximumNumberOfLines = 0
        label.translatesAutoresizingMaskIntoConstraints = false

        if role == .assistant {
            let avatar = AvatarView(diameter: 24, fontSize: 10)
            addSubview(avatar)
            addSubview(label)
            label.preferredMaxLayoutWidth = 600
            NSLayoutConstraint.activate([
                avatar.leadingAnchor.constraint(equalTo: leadingAnchor),
                avatar.topAnchor.constraint(equalTo: topAnchor, constant: 1),
                label.leadingAnchor.constraint(equalTo: avatar.trailingAnchor, constant: 10),
                label.topAnchor.constraint(equalTo: topAnchor, constant: 1),
                label.widthAnchor.constraint(lessThanOrEqualToConstant: 600),
                label.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
                label.bottomAnchor.constraint(equalTo: bottomAnchor),
                avatar.bottomAnchor.constraint(lessThanOrEqualTo: bottomAnchor),
            ])
            setText(message.text)
            return
        }

        bubble.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
        bubble.addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: bubble.leadingAnchor, constant: 11),
            label.trailingAnchor.constraint(equalTo: bubble.trailingAnchor, constant: -11),
            label.topAnchor.constraint(equalTo: bubble.topAnchor, constant: 7),
            label.bottomAnchor.constraint(equalTo: bubble.bottomAnchor, constant: -7),
        ])
        label.preferredMaxLayoutWidth = 500
        addSubview(bubble)
        NSLayoutConstraint.activate([
            bubble.topAnchor.constraint(equalTo: topAnchor),
            bubble.trailingAnchor.constraint(equalTo: trailingAnchor),
            bubble.bottomAnchor.constraint(equalTo: bottomAnchor),
            bubble.widthAnchor.constraint(lessThanOrEqualToConstant: 540),
            bubble.leadingAnchor.constraint(greaterThanOrEqualTo: leadingAnchor, constant: 80),
        ])
        setText(message.text)
    }

    required init?(coder: NSCoder) { fatalError() }

    private func setText(_ text: String) {
        label?.attributedStringValue = MarkdownRenderer.render(text, isUser: role == .user)
    }

    func update(_ message: ChatMessage) {
        setText(message.text)
    }
}

final class TypingRowView: NSView {
    private let spinner = NSProgressIndicator()
    private let label = NSTextField(labelWithString: "正在想…")

    init() {
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        let avatar = AvatarView(diameter: 24, fontSize: 10)
        addSubview(avatar)

        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.isIndeterminate = true
        spinner.startAnimation(nil)
        spinner.translatesAutoresizingMaskIntoConstraints = false
        addSubview(spinner)

        label.font = .systemFont(ofSize: 11)
        label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)

        NSLayoutConstraint.activate([
            avatar.leadingAnchor.constraint(equalTo: leadingAnchor),
            avatar.topAnchor.constraint(equalTo: topAnchor, constant: 1),
            spinner.leadingAnchor.constraint(equalTo: avatar.trailingAnchor, constant: 10),
            spinner.centerYAnchor.constraint(equalTo: label.centerYAnchor),
            spinner.widthAnchor.constraint(equalToConstant: 14),
            spinner.heightAnchor.constraint(equalToConstant: 14),
            label.leadingAnchor.constraint(equalTo: spinner.trailingAnchor, constant: 6),
            label.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }
}

// MARK: - 侧栏

final class SessionCellView: NSTableCellView {
    private let preview = NSTextField(labelWithString: "")
    private let meta = NSTextField(labelWithString: "")

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        preview.font = .systemFont(ofSize: 13)
        preview.lineBreakMode = .byTruncatingTail
        preview.translatesAutoresizingMaskIntoConstraints = false
        meta.font = .systemFont(ofSize: 11)
        meta.textColor = .secondaryLabelColor
        meta.translatesAutoresizingMaskIntoConstraints = false
        addSubview(preview)
        addSubview(meta)
        NSLayoutConstraint.activate([
            preview.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 2),
            preview.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -2),
            preview.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            meta.leadingAnchor.constraint(equalTo: preview.leadingAnchor),
            meta.trailingAnchor.constraint(equalTo: preview.trailingAnchor),
            meta.topAnchor.constraint(equalTo: preview.bottomAnchor, constant: 1),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(_ session: ChatSession) {
        if session.hasHistory {
            preview.stringValue = session.preview.isEmpty ? "（无预览）" : session.preview
            preview.textColor = .labelColor
        } else {
            preview.stringValue = "旧会话"
            preview.textColor = .secondaryLabelColor
        }
        meta.stringValue = metaText(for: session)
    }

    private func metaText(for session: ChatSession) -> String {
        if session.hasHistory { return "\(session.turns) 轮" }
        let f = DateFormatter()
        f.dateFormat = "M月d日 HH:mm"
        return "\(session.turns) 轮 · \(f.string(from: Date(timeIntervalSince1970: session.updatedAt / 1000)))"
    }

    override var backgroundStyle: NSView.BackgroundStyle {
        didSet {
            let strong = backgroundStyle == .emphasized
            preview.textColor = strong ? .alternateSelectedControlTextColor : (preview.textColor == .secondaryLabelColor ? .secondaryLabelColor : .labelColor)
            meta.textColor = strong ? .alternateSelectedControlTextColor.withAlphaComponent(0.85) : .secondaryLabelColor
        }
    }
}

final class SidebarViewController: NSViewController, NSOutlineViewDataSource, NSOutlineViewDelegate {
    private let service: ChatService
    private var outlineView: NSOutlineView!
    private var nodes: [Node] = []
    private var suppressSelection = false
    private var searchText = ""

    private struct Node {
        let group: String?
        let session: ChatSession?
    }

    init(service: ChatService) {
        self.service = service
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let effect = NSVisualEffectView()
        effect.material = .sidebar
        effect.blendingMode = .behindWindow
        effect.state = .followsWindowActiveState
        view = effect

        let table = NSOutlineView()
        table.headerView = nil
        table.style = .sourceList
        table.selectionHighlightStyle = .regular
        table.rowSizeStyle = .default
        table.floatsGroupRows = false
        table.dataSource = self
        table.delegate = self
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("session"))
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
        view.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: view.topAnchor),
            scroll.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.bottomAnchor),
        ])
    }

    func update(sessions: [ChatSession], selected: String?, search: String) {
        searchText = search
        let filtered = search.isEmpty
            ? sessions
            : sessions.filter { $0.preview.localizedCaseInsensitiveContains(search) || $0.id.localizedCaseInsensitiveContains(search) }
        var grouped: [String: [ChatSession]] = [:]
        let calendar = Calendar.current
        for session in filtered {
            let date = Date(timeIntervalSince1970: session.updatedAt / 1000)
            let group: String
            if calendar.isDateInToday(date) { group = "今天" }
            else if calendar.isDateInYesterday(date) { group = "昨天" }
            else { group = "更早" }
            grouped[group, default: []].append(session)
        }
        var built: [Node] = []
        for group in ["今天", "昨天", "更早"] {
            guard let items = grouped[group], !items.isEmpty else { continue }
            built.append(Node(group: group, session: nil))
            built.append(contentsOf: items.map { Node(group: nil, session: $0) })
        }
        nodes = built
        suppressSelection = true
        outlineView.reloadData()
        if let selected, let index = nodes.firstIndex(where: { $0.session?.id == selected }) {
            outlineView.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false)
            outlineView.scrollRowToVisible(index)
        }
        suppressSelection = false
    }

    func numberOfChildren(of item: Any?) -> Int {
        item == nil ? nodes.count : 0
    }

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        numberOfChildren(of: item)
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        nodes[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool { false }

    func outlineView(_ outlineView: NSOutlineView, isGroupItem item: Any) -> Bool {
        (item as? Node)?.group != nil
    }

    func outlineView(_ outlineView: NSOutlineView, heightOfRowByItem item: Any) -> CGFloat {
        (item as? Node)?.group != nil ? 22 : 40
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let node = item as? Node else { return nil }
        if let group = node.group {
            let cell = NSTableCellView()
            let label = NSTextField(labelWithString: group)
            label.font = .systemFont(ofSize: 11, weight: .semibold)
            label.textColor = .secondaryLabelColor
            label.translatesAutoresizingMaskIntoConstraints = false
            cell.addSubview(label)
            NSLayoutConstraint.activate([
                label.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 4),
                label.bottomAnchor.constraint(equalTo: cell.bottomAnchor, constant: -2),
            ])
            return cell
        }
        guard let session = node.session else { return nil }
        let cell = (outlineView.makeView(withIdentifier: NSUserInterfaceItemIdentifier("session-cell"), owner: self) as? SessionCellView) ?? SessionCellView()
        cell.identifier = NSUserInterfaceItemIdentifier("session-cell")
        cell.configure(session)
        return cell
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        guard !suppressSelection else { return }
        let row = outlineView.selectedRow
        guard row >= 0, row < nodes.count, let session = nodes[row].session else { return }
        service.openSession(session.id)
    }
}

// MARK: - 主内容

final class ChatViewController: NSViewController, NSTextViewDelegate {
    private let service: ChatService
    private var scrollView: NSScrollView!
    private var document: FlippedView!
    private var contentColumn: NSView!
    private var stack: NSStackView!
    private var emptyState: NSStackView!
    private var composer: NSView!
    private var composerTextView: NSTextView!
    private var composerPlaceholder = NSTextField(labelWithString: "说点什么…")
    private var composerHeight: NSLayoutConstraint!
    private var textHeight: NSLayoutConstraint!
    private var sendButton: NSButton!
    private var approvalCard: NSView!
    private var approvalTitle = NSTextField(labelWithString: "需要你确认")
    private var approvalDetail = NSTextField(labelWithString: "")
    private var approvalPreview = NSTextField(wrappingLabelWithString: "")

    private var rows: [MessageRowView] = []
    private var renderedIDs: [UUID] = []
    private var typingRow: TypingRowView?
    private var cancellables = Set<AnyCancellable>()

    var chatService: ChatService { service }

    init(service: ChatService) {
        self.service = service
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let root = NSView()
        root.wantsLayer = true
        root.layer?.backgroundColor = NSColor.textBackgroundColor.cgColor
        view = root

        scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(scrollView)

        document = FlippedView()
        document.translatesAutoresizingMaskIntoConstraints = false
        scrollView.documentView = document

        contentColumn = NSView()
        contentColumn.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(contentColumn)

        stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentColumn.addSubview(stack)

        let preferred = contentColumn.widthAnchor.constraint(equalTo: document.widthAnchor, constant: -56)
        preferred.priority = .defaultHigh
        NSLayoutConstraint.activate([
            document.widthAnchor.constraint(equalTo: scrollView.contentView.widthAnchor),
            contentColumn.topAnchor.constraint(equalTo: document.topAnchor, constant: 18),
            contentColumn.bottomAnchor.constraint(equalTo: document.bottomAnchor, constant: -12),
            contentColumn.centerXAnchor.constraint(equalTo: document.centerXAnchor),
            contentColumn.widthAnchor.constraint(lessThanOrEqualToConstant: 640),
            contentColumn.leadingAnchor.constraint(greaterThanOrEqualTo: document.leadingAnchor, constant: 24),
            contentColumn.trailingAnchor.constraint(lessThanOrEqualTo: document.trailingAnchor, constant: -24),
            preferred,
            stack.topAnchor.constraint(equalTo: contentColumn.topAnchor),
            stack.leadingAnchor.constraint(equalTo: contentColumn.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: contentColumn.trailingAnchor),
            stack.bottomAnchor.constraint(equalTo: contentColumn.bottomAnchor),
        ])

        buildEmptyState(in: root)
        buildComposer(in: root)
        buildApprovalCard(in: root)
        bind()
    }

    private func buildEmptyState(in root: NSView) {
        let icon = NSImageView()
        icon.image = symbol("bubble.left.and.bubble.right", size: 34)
        icon.contentTintColor = .tertiaryLabelColor
        icon.translatesAutoresizingMaskIntoConstraints = false

        let title = NSTextField(labelWithString: "今天想聊点什么？")
        title.font = .systemFont(ofSize: 20, weight: .semibold)
        let sub = NSTextField(labelWithString: "说事、闲聊、安排今天，都可以。")
        sub.font = .systemFont(ofSize: 13)
        sub.textColor = .secondaryLabelColor

        emptyState = NSStackView(views: [icon, title, sub])
        emptyState.orientation = .vertical
        emptyState.alignment = .centerX
        emptyState.spacing = 8
        emptyState.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(emptyState)
        NSLayoutConstraint.activate([
            icon.widthAnchor.constraint(equalToConstant: 38),
            icon.heightAnchor.constraint(equalToConstant: 38),
            emptyState.centerXAnchor.constraint(equalTo: root.centerXAnchor),
            emptyState.centerYAnchor.constraint(equalTo: root.centerYAnchor, constant: -16),
        ])
    }

    private func buildComposer(in root: NSView) {
        let container = NSView()
        container.wantsLayer = true
        container.layer?.cornerRadius = 10
        container.layer?.cornerCurve = .continuous
        container.layer?.backgroundColor = NSColor.controlBackgroundColor.cgColor
        container.layer?.borderWidth = 1
        container.layer?.borderColor = NSColor.separatorColor.cgColor
        container.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(container)
        composer = container

        let textScroll = NSScrollView()
        textScroll.hasVerticalScroller = false
        textScroll.drawsBackground = false
        textScroll.borderType = .noBorder
        textScroll.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(textScroll)

        composerTextView = NSTextView()
        composerTextView.isRichText = false
        composerTextView.isEditable = true
        composerTextView.isSelectable = true
        composerTextView.drawsBackground = false
        composerTextView.font = .systemFont(ofSize: 13)
        composerTextView.textColor = .labelColor
        composerTextView.insertionPointColor = .controlAccentColor
        composerTextView.textContainerInset = NSSize(width: 0, height: 3)
        composerTextView.isVerticallyResizable = true
        composerTextView.isHorizontallyResizable = false
        composerTextView.autoresizingMask = [.width]
        composerTextView.textContainer?.widthTracksTextView = true
        composerTextView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        composerTextView.delegate = self
        textScroll.documentView = composerTextView

        composerPlaceholder.font = .systemFont(ofSize: 13)
        composerPlaceholder.textColor = .tertiaryLabelColor
        composerPlaceholder.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(composerPlaceholder)

        sendButton = NSButton()
        sendButton.image = symbol("arrow.up.circle.fill", size: 17)
        sendButton.contentTintColor = .controlAccentColor
        sendButton.isBordered = false
        sendButton.target = self
        sendButton.action = #selector(sendMessage)
        sendButton.toolTip = "发送（Return）"
        sendButton.setAccessibilityLabel("发送")
        sendButton.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(sendButton)

        composerHeight = container.heightAnchor.constraint(equalToConstant: 40)
        composerHeight.isActive = true
        textHeight = textScroll.heightAnchor.constraint(equalToConstant: 20)
        textHeight.isActive = true

        NSLayoutConstraint.activate([
            container.leadingAnchor.constraint(greaterThanOrEqualTo: root.leadingAnchor, constant: 32),
            container.trailingAnchor.constraint(lessThanOrEqualTo: root.trailingAnchor, constant: -32),
            container.centerXAnchor.constraint(equalTo: root.centerXAnchor),
            container.widthAnchor.constraint(lessThanOrEqualToConstant: 640),
            container.widthAnchor.constraint(greaterThanOrEqualToConstant: 460),
            container.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -18),

            scrollView.topAnchor.constraint(equalTo: root.topAnchor),
            scrollView.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scrollView.bottomAnchor.constraint(equalTo: container.topAnchor, constant: -12),

            textScroll.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 11),
            textScroll.trailingAnchor.constraint(equalTo: sendButton.leadingAnchor, constant: -8),
            textScroll.topAnchor.constraint(equalTo: container.topAnchor, constant: 8),
            textScroll.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -8),

            composerPlaceholder.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 12),
            composerPlaceholder.topAnchor.constraint(equalTo: textScroll.topAnchor, constant: 4),

            sendButton.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -8),
            sendButton.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -7),
            sendButton.widthAnchor.constraint(equalToConstant: 24),
            sendButton.heightAnchor.constraint(equalToConstant: 24),
        ])
    }

    private func buildApprovalCard(in root: NSView) {
        let card = NSView()
        card.wantsLayer = true
        card.layer?.cornerRadius = 8
        card.layer?.cornerCurve = .continuous
        card.layer?.backgroundColor = NSColor.controlBackgroundColor.cgColor
        card.layer?.borderWidth = 1
        card.layer?.borderColor = NSColor.separatorColor.cgColor
        card.isHidden = true
        card.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(card)
        approvalCard = card

        approvalTitle.font = .systemFont(ofSize: 13, weight: .semibold)
        approvalDetail.font = .systemFont(ofSize: 11)
        approvalDetail.textColor = .secondaryLabelColor
        approvalPreview.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        approvalPreview.textColor = .secondaryLabelColor
        approvalPreview.maximumNumberOfLines = 5
        approvalPreview.preferredMaxLayoutWidth = 560

        let allow = NSButton(title: "允许", target: self, action: #selector(approveAction))
        allow.bezelStyle = .rounded
        allow.controlSize = .small
        let deny = NSButton(title: "拒绝", target: self, action: #selector(denyAction))
        deny.bezelStyle = .rounded
        deny.controlSize = .small
        let buttons = NSStackView(views: [NSView(), deny, allow])
        buttons.orientation = .horizontal
        buttons.spacing = 6
        buttons.translatesAutoresizingMaskIntoConstraints = false

        let content = NSStackView(views: [approvalTitle, approvalDetail, approvalPreview, buttons])
        content.orientation = .vertical
        content.alignment = .leading
        content.spacing = 4
        content.translatesAutoresizingMaskIntoConstraints = false
        card.addSubview(content)
        NSLayoutConstraint.activate([
            card.leadingAnchor.constraint(equalTo: composer.leadingAnchor),
            card.trailingAnchor.constraint(equalTo: composer.trailingAnchor),
            card.bottomAnchor.constraint(equalTo: composer.topAnchor, constant: -8),
            content.leadingAnchor.constraint(equalTo: card.leadingAnchor, constant: 10),
            content.trailingAnchor.constraint(equalTo: card.trailingAnchor, constant: -10),
            content.topAnchor.constraint(equalTo: card.topAnchor, constant: 8),
            content.bottomAnchor.constraint(equalTo: card.bottomAnchor, constant: -8),
        ])
    }

    private func bind() {
        service.$messages.receive(on: RunLoop.main).sink { [weak self] _ in self?.render() }.store(in: &cancellables)
        service.$busy.receive(on: RunLoop.main).sink { [weak self] _ in self?.render() }.store(in: &cancellables)
        service.$pendingApproval.receive(on: RunLoop.main).sink { [weak self] in self?.updateApproval($0) }.store(in: &cancellables)
        service.$connected.receive(on: RunLoop.main).sink { [weak self] connected in
            self?.composerTextView.isEditable = connected
            self?.sendButton.isEnabled = connected
        }.store(in: &cancellables)
    }

    private func render() {
        let messages = service.messages
        let ids = messages.map(\.id)
        if ids.count < renderedIDs.count || !ids.starts(with: renderedIDs) {
            for row in rows { row.removeFromSuperview() }
            rows.removeAll()
            typingRow?.removeFromSuperview()
            typingRow = nil
            for message in messages { appendRow(message) }
            renderedIDs = ids
        } else {
            if ids.count > renderedIDs.count {
                for index in renderedIDs.count..<messages.count { appendRow(messages[index]) }
                renderedIDs = ids
            }
            for (index, row) in rows.enumerated() where index < messages.count { row.update(messages[index]) }
        }
        emptyState.isHidden = messages.contains { $0.role != .system }
        updateTyping(visible: service.busy)
        scrollToBottom()
    }

    private func appendRow(_ message: ChatMessage) {
        let row = MessageRowView(message: message)
        row.alphaValue = 0
        stack.addArrangedSubview(row)
        row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        rows.append(row)
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0.16
            row.animator().alphaValue = 1
        }
    }

    private func updateTyping(visible: Bool) {
        if visible, typingRow == nil {
            let row = TypingRowView()
            stack.addArrangedSubview(row)
            row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
            typingRow = row
        } else if !visible, let typingRow {
            typingRow.removeFromSuperview()
            self.typingRow = nil
        }
    }

    private func scrollToBottom() {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.document.layoutSubtreeIfNeeded()
            let height = self.stack.fittingSize.height
            let visible = self.scrollView.contentView.bounds.height
            self.scrollView.contentView.scroll(to: NSPoint(x: 0, y: max(0, height - visible + 24)))
            self.scrollView.reflectScrolledClipView(self.scrollView.contentView)
        }
    }

    private func updateApproval(_ approval: PendingApproval?) {
        guard let approval else {
            approvalCard.isHidden = true
            return
        }
        approvalTitle.stringValue = "需要你确认 · \(approval.tool)（\(approval.capability)）"
        var detail = approval.reason
        if let specifier = approval.specifier, !specifier.isEmpty { detail += " · 对象：\(specifier)" }
        approvalDetail.stringValue = detail
        let body = approval.preview ?? approval.arguments
        approvalPreview.stringValue = body.isEmpty ? "（无预览）" : body
        approvalCard.isHidden = false
    }

    @objc private func approveAction() {
        guard let approval = service.pendingApproval else { return }
        service.respondApproval(approval.id, allow: true)
    }

    @objc private func denyAction() {
        guard let approval = service.pendingApproval else { return }
        service.respondApproval(approval.id, allow: false)
    }

    func textDidChange(_ notification: Notification) {
        let text = composerTextView.string
        composerPlaceholder.isHidden = !text.isEmpty
        let used = composerTextView.layoutManager?.usedRect(for: composerTextView.textContainer!) ?? .zero
        let height = min(110, max(20, used.height + 6))
        if abs(textHeight.constant - height) > 0.5 {
            textHeight.constant = height
            composerHeight.constant = height + 20
            view.window?.contentView?.layoutSubtreeIfNeeded()
        }
        sendButton.isEnabled = service.connected && !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        composerTextView.scrollRangeToVisible(composerTextView.selectedRange())
    }

    func textView(_ textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            if NSEvent.modifierFlags.contains(.shift) {
                textView.insertNewlineIgnoringFieldEditor(nil)
                return true
            }
            sendMessage()
            return true
        }
        return false
    }

    @objc private func sendMessage() {
        let text = composerTextView.string.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, service.connected, !service.busy else { return }
        composerTextView.string = ""
        composerPlaceholder.isHidden = false
        textHeight.constant = 20
        composerHeight.constant = 40
        sendButton.isEnabled = false
        service.send(text)
        view.window?.makeFirstResponder(composerTextView)
    }

    func focusComposer() {
        view.window?.makeFirstResponder(composerTextView)
    }
}

// MARK: - App

final class MacAppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSToolbarDelegate {
    private let service = ChatService()
    private var window: NSWindow!
    private var splitVC: NSSplitViewController!
    private var sidebarVC: SidebarViewController!
    private var chatVC: ChatViewController!
    private var searchField: NSSearchField!
    private var effortPopup: NSPopUpButton!
    private var capabilityWindow: CapabilityWindowController?
    private var cancellables = Set<AnyCancellable>()

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.applicationIconImage = Self.makeIcon()
        buildMenu()
        buildWindow()
        bind()
        service.start()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        chatVC.focusComposer()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
    func applicationWillTerminate(_ notification: Notification) { service.stop() }

    private func buildWindow() {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1040, height: 720),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙"
        window.subtitle = "连接中…"
        window.minSize = NSSize(width: 760, height: 520)
        window.collectionBehavior.insert(.fullScreenPrimary)
        window.center()
        window.delegate = self
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("YunXiMacMainV2")

        splitVC = NSSplitViewController()
        splitVC.splitView.dividerStyle = .thin
        sidebarVC = SidebarViewController(service: service)
        chatVC = ChatViewController(service: service)

        let sidebarItem = NSSplitViewItem(sidebarWithViewController: sidebarVC)
        sidebarItem.canCollapse = true
        sidebarItem.minimumThickness = 200
        sidebarItem.maximumThickness = 320
        sidebarItem.preferredThicknessFraction = 0.24
        splitVC.addSplitViewItem(sidebarItem)
        splitVC.addSplitViewItem(NSSplitViewItem(viewController: chatVC))
        window.contentViewController = splitVC
        window.setContentSize(NSSize(width: 1040, height: 720))
        window.center()

        let toolbar = NSToolbar(identifier: "YunXiMainToolbar")
        toolbar.delegate = self
        toolbar.displayMode = .iconOnly
        toolbar.allowsUserCustomization = true
        toolbar.autosavesConfiguration = true
        window.toolbar = toolbar
        window.toolbarStyle = .unified
    }

    private func bind() {
        service.$sessions.receive(on: RunLoop.main).sink { [weak self] sessions in
            guard let self else { return }
            self.sidebarVC.update(sessions: sessions, selected: self.service.session, search: self.searchField?.stringValue ?? "")
        }.store(in: &cancellables)
        service.$session.receive(on: RunLoop.main).sink { [weak self] _ in
            guard let self else { return }
            self.sidebarVC.update(sessions: self.service.sessions, selected: self.service.session, search: self.searchField?.stringValue ?? "")
        }.store(in: &cancellables)
        service.$status.receive(on: RunLoop.main).sink { [weak self] status in
            self?.window.subtitle = status
        }.store(in: &cancellables)
        service.$effort.receive(on: RunLoop.main).sink { [weak self] effort in
            let index = ["auto": 0, "on": 1, "off": 2][effort] ?? 0
            self?.effortPopup?.selectItem(at: index)
        }.store(in: &cancellables)
    }

    // MARK: Toolbar

    private enum Item {
        static let newChat = NSToolbarItem.Identifier("newChat")
        static let search = NSToolbarItem.Identifier("search")
        static let effort = NSToolbarItem.Identifier("effort")
        static let capability = NSToolbarItem.Identifier("capability")
    }

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleSidebar, .flexibleSpace, Item.newChat, Item.search, Item.effort, Item.capability]
    }

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleSidebar, .flexibleSpace, .space, Item.newChat, Item.search, Item.effort, Item.capability]
    }

    func toolbar(_ toolbar: NSToolbar, itemForItemIdentifier itemIdentifier: NSToolbarItem.Identifier, willBeInsertedIntoToolbar flag: Bool) -> NSToolbarItem? {
        switch itemIdentifier {
        case .toggleSidebar:
            let item = NSToolbarItem(itemIdentifier: .toggleSidebar)
            item.label = "边栏"
            return item
        case Item.newChat:
            let item = NSToolbarItem(itemIdentifier: itemIdentifier)
            item.label = "新会话"
            item.image = symbol("square.and.pencil", size: 15)
            item.target = self
            item.action = #selector(newChat)
            return item
        case Item.capability:
            let item = NSToolbarItem(itemIdentifier: itemIdentifier)
            item.label = "能力"
            item.image = symbol("square.grid.2x2", size: 15)
            item.target = self
            item.action = #selector(showCapabilities)
            return item
        case Item.search:
            let item = NSToolbarItem(itemIdentifier: itemIdentifier)
            let field = NSSearchField()
            field.placeholderString = "搜索会话"
            field.font = .systemFont(ofSize: 12)
            field.controlSize = .small
            field.target = self
            field.action = #selector(searchChanged)
            field.sendsSearchStringImmediately = true
            field.translatesAutoresizingMaskIntoConstraints = false
            field.widthAnchor.constraint(equalToConstant: 180).isActive = true
            searchField = field
            item.label = "搜索"
            item.view = field
            return item
        case Item.effort:
            let item = NSToolbarItem(itemIdentifier: itemIdentifier)
            let popup = NSPopUpButton(frame: .zero, pullsDown: false)
            popup.addItems(withTitles: ["自动思考", "始终思考", "不思考"])
            popup.controlSize = .small
            popup.font = .systemFont(ofSize: 11)
            popup.target = self
            popup.action = #selector(effortChanged)
            popup.translatesAutoresizingMaskIntoConstraints = false
            popup.widthAnchor.constraint(equalToConstant: 92).isActive = true
            effortPopup = popup
            item.label = "思考"
            item.view = popup
            return item
        default:
            return nil
        }
    }

    // MARK: Menu

    private func buildMenu() {
        let main = NSMenu()

        let appItem = NSMenuItem()
        main.addItem(appItem)
        let app = NSMenu()
        appItem.submenu = app
        app.addItem(withTitle: "关于 云熙", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        app.addItem(.separator())
        let settings = NSMenuItem(title: "设置…", action: #selector(showCapabilities), keyEquivalent: ",")
        settings.target = self
        app.addItem(settings)
        app.addItem(.separator())
        let services = NSMenu()
        let servicesItem = NSMenuItem(title: "服务", action: nil, keyEquivalent: "")
        servicesItem.submenu = services
        app.addItem(servicesItem)
        NSApp.servicesMenu = services
        app.addItem(.separator())
        app.addItem(withTitle: "隐藏 云熙", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        let hideOthers = NSMenuItem(title: "隐藏其他", action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        app.addItem(hideOthers)
        app.addItem(withTitle: "全部显示", action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: "退出 云熙", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        let fileItem = NSMenuItem()
        main.addItem(fileItem)
        let file = NSMenu(title: "文件")
        fileItem.submenu = file
        let new = NSMenuItem(title: "新会话", action: #selector(newChat), keyEquivalent: "n")
        new.target = self
        file.addItem(new)
        file.addItem(withTitle: "关闭窗口", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")

        let editItem = NSMenuItem()
        main.addItem(editItem)
        let edit = NSMenu(title: "编辑")
        editItem.submenu = edit
        edit.addItem(withTitle: "撤销", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = NSMenuItem(title: "重做", action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        edit.addItem(redo)
        edit.addItem(.separator())
        edit.addItem(withTitle: "剪切", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "复制", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "粘贴", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "全选", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        edit.addItem(.separator())
        let find = NSMenuItem(title: "查找会话", action: #selector(focusSearch), keyEquivalent: "f")
        find.target = self
        edit.addItem(find)

        let viewItem = NSMenuItem()
        main.addItem(viewItem)
        let viewMenu = NSMenu(title: "显示")
        viewItem.submenu = viewMenu
        let toggle = NSMenuItem(title: "显示/隐藏边栏", action: #selector(toggleSidebar), keyEquivalent: "s")
        toggle.keyEquivalentModifierMask = [.command, .control]
        toggle.target = self
        viewMenu.addItem(toggle)
        viewMenu.addItem(.separator())
        let fullScreen = NSMenuItem(title: "进入全屏幕", action: #selector(NSWindow.toggleFullScreen(_:)), keyEquivalent: "f")
        fullScreen.keyEquivalentModifierMask = [.command, .control]
        viewMenu.addItem(fullScreen)

        let windowItem = NSMenuItem()
        main.addItem(windowItem)
        let windowMenu = NSMenu(title: "窗口")
        windowItem.submenu = windowMenu
        windowMenu.addItem(withTitle: "最小化", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "缩放", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowMenu.addItem(.separator())
        windowMenu.addItem(withTitle: "前置全部窗口", action: #selector(NSApplication.arrangeInFront(_:)), keyEquivalent: "")
        NSApp.windowsMenu = windowMenu

        let helpItem = NSMenuItem()
        main.addItem(helpItem)
        let help = NSMenu(title: "帮助")
        helpItem.submenu = help
        let readme = NSMenuItem(title: "打开项目说明", action: #selector(openHelp), keyEquivalent: "")
        readme.target = self
        help.addItem(readme)

        NSApp.mainMenu = main
    }

    // MARK: Actions

    @objc private func newChat() {
        service.newChat()
        chatVC.focusComposer()
    }

    @objc private func showCapabilities() {
        if capabilityWindow == nil {
            capabilityWindow = CapabilityWindowController(service: service)
        }
        capabilityWindow?.showWindow(nil)
        capabilityWindow?.window?.makeKeyAndOrderFront(nil)
        service.requestCapabilities()
    }

    @objc private func toggleSidebar() {
        splitVC.toggleSidebar(nil)
    }

    @objc private func focusSearch() {
        if let searchField { window.makeFirstResponder(searchField) }
    }

    @objc private func searchChanged() {
        sidebarVC.update(sessions: service.sessions, selected: service.session, search: searchField.stringValue)
    }

    @objc private func effortChanged() {
        let values = ["auto", "on", "off"]
        let index = effortPopup.indexOfSelectedItem
        if index >= 0 && index < values.count { service.setEffort(values[index]) }
    }

    @objc private func openHelp() {
        let readme = URL(fileURLWithPath: FileManager.default.currentDirectoryPath).appendingPathComponent("README.md")
        NSWorkspace.shared.open(readme)
    }

    private static func makeIcon() -> NSImage {
        let size = NSSize(width: 512, height: 512)
        let image = NSImage(size: size)
        image.lockFocus()
        defer { image.unlockFocus() }
        let rect = NSRect(origin: .zero, size: size)
        let path = NSBezierPath(roundedRect: rect.insetBy(dx: 26, dy: 26), xRadius: 112, yRadius: 112)
        let gradient = NSGradient(colors: [
            NSColor(calibratedRed: 0.30, green: 0.12, blue: 0.17, alpha: 1),
            NSColor(calibratedRed: 0.62, green: 0.28, blue: 0.38, alpha: 1),
            NSColor(calibratedRed: 0.80, green: 0.45, blue: 0.52, alpha: 1),
        ])
        gradient?.draw(in: path, angle: -70)
        let text = "熙" as NSString
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 250, weight: .thin),
            .foregroundColor: NSColor.white.withAlphaComponent(0.94),
        ]
        let textSize = text.size(withAttributes: attrs)
        text.draw(at: NSPoint(x: (size.width - textSize.width) / 2, y: (size.height - textSize.height) / 2 - 6), withAttributes: attrs)
        return image
    }
}
