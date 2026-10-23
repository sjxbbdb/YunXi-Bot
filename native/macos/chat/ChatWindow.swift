import AppKit
import Combine

// MARK: - 设计 token（参考 Codex 前端：spacing 4、composer 44/22、elevation）

extension NSColor {
    static let yunxiAccent = NSColor(calibratedRed: 0.66, green: 0.33, blue: 0.42, alpha: 1.0)

    static func adaptive(light: NSColor, dark: NSColor) -> NSColor {
        NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
        }
    }

    static let bubbleUser = NSColor.adaptive(
        light: NSColor(calibratedRed: 0.72, green: 0.36, blue: 0.45, alpha: 1.0),
        dark: NSColor(calibratedRed: 0.54, green: 0.25, blue: 0.33, alpha: 1.0)
    )
    static let bubbleAssistant = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.955, alpha: 1.0),
        dark: NSColor(calibratedWhite: 0.185, alpha: 1.0)
    )
    static let bubbleBorder = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.87, alpha: 1.0),
        dark: NSColor(calibratedWhite: 0.29, alpha: 1.0)
    )
    static let composerSurface = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.995, alpha: 1.0),
        dark: NSColor(calibratedWhite: 0.165, alpha: 1.0)
    )
    static let cardSurface = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.985, alpha: 1.0),
        dark: NSColor(calibratedWhite: 0.21, alpha: 1.0)
    )
}

final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

// MARK: - 头像

final class AvatarView: NSView {
    init(diameter: CGFloat, fontSize: CGFloat) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = diameter / 2
        layer?.backgroundColor = NSColor.yunxiAccent.cgColor
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

// MARK: - 消息气泡

final class BubbleView: NSView {
    let label = NSTextField(wrappingLabelWithString: "")
    private var plainText = ""

    init(isUser: Bool) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 16
        layer?.backgroundColor = (isUser ? NSColor.bubbleUser : NSColor.bubbleAssistant).cgColor
        layer?.borderWidth = isUser ? 0 : 1
        layer?.borderColor = NSColor.bubbleBorder.cgColor

        label.isSelectable = true
        label.maximumNumberOfLines = 0
        label.preferredMaxLayoutWidth = 340
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            label.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            label.topAnchor.constraint(equalTo: topAnchor, constant: 8),
            label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -8),
            label.widthAnchor.constraint(lessThanOrEqualToConstant: 340),
        ])
        translatesAutoresizingMaskIntoConstraints = false

        let menu = NSMenu()
        let copyItem = NSMenuItem(title: "复制", action: #selector(copyText), keyEquivalent: "")
        copyItem.target = self
        menu.addItem(copyItem)
        label.menu = menu
    }

    required init?(coder: NSCoder) { fatalError() }

    @objc private func copyText() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(plainText, forType: .string)
    }

    func update(text: String, isUser: Bool) {
        plainText = text
        label.attributedStringValue = MarkdownRenderer.render(text, isUser: isUser)
    }
}

// MARK: - 消息行

final class MessageRowView: NSView {
    let bubble: BubbleView?
    let textLabel: NSTextField?
    private let role: ChatMessage.Role

    private static let timeFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()
    private static let dayFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "M月d日 HH:mm"
        return f
    }()

    init(message: ChatMessage, showDate: Bool) {
        role = message.role
        if message.role == .system {
            bubble = nil
            textLabel = NSTextField(wrappingLabelWithString: message.text)
        } else {
            bubble = BubbleView(isUser: message.role == .user)
            textLabel = nil
            bubble?.update(text: message.text, isUser: message.role == .user)
        }
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false

        if let textLabel {
            textLabel.font = .systemFont(ofSize: 11)
            textLabel.textColor = .secondaryLabelColor
            textLabel.alignment = .center
            textLabel.translatesAutoresizingMaskIntoConstraints = false
            addSubview(textLabel)
            NSLayoutConstraint.activate([
                textLabel.topAnchor.constraint(equalTo: topAnchor, constant: 6),
                textLabel.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
                textLabel.leadingAnchor.constraint(greaterThanOrEqualTo: leadingAnchor, constant: 40),
                textLabel.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -40),
                textLabel.centerXAnchor.constraint(equalTo: centerXAnchor),
            ])
            return
        }

        guard let bubble else { return }
        let timeText: String
        if let at = message.at {
            timeText = showDate ? Self.dayFormatter.string(from: at) : Self.timeFormatter.string(from: at)
        } else {
            timeText = ""
        }
        let time = NSTextField(labelWithString: timeText)
        time.font = .systemFont(ofSize: 9.5)
        time.textColor = .tertiaryLabelColor
        time.isHidden = timeText.isEmpty
        time.translatesAutoresizingMaskIntoConstraints = false

        let column = NSStackView(views: [bubble, time])
        column.orientation = .vertical
        column.alignment = role == .user ? .trailing : .leading
        column.spacing = 3
        column.translatesAutoresizingMaskIntoConstraints = false
        addSubview(column)

        if role == .assistant {
            let avatar = AvatarView(diameter: 26, fontSize: 11)
            addSubview(avatar)
            NSLayoutConstraint.activate([
                avatar.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
                avatar.topAnchor.constraint(equalTo: topAnchor, constant: 2),
                column.leadingAnchor.constraint(equalTo: avatar.trailingAnchor, constant: 8),
                column.topAnchor.constraint(equalTo: topAnchor, constant: 2),
                column.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
                column.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -60),
            ])
        } else {
            NSLayoutConstraint.activate([
                column.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
                column.topAnchor.constraint(equalTo: topAnchor, constant: 2),
                column.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
                column.leadingAnchor.constraint(greaterThanOrEqualTo: leadingAnchor, constant: 70),
            ])
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    func update(_ message: ChatMessage) {
        if let bubble {
            bubble.update(text: message.text, isUser: message.role == .user)
        } else if let textLabel {
            if textLabel.stringValue != message.text { textLabel.stringValue = message.text }
        }
    }
}

// MARK: - 打字指示

final class TypingRowView: NSView {
    private let dot = NSView()

    init() {
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        dot.layer?.backgroundColor = NSColor.yunxiAccent.cgColor
        dot.translatesAutoresizingMaskIntoConstraints = false
        addSubview(dot)

        let label = NSTextField(labelWithString: "云熙正在想…")
        label.font = .systemFont(ofSize: 11)
        label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)

        NSLayoutConstraint.activate([
            dot.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 48),
            dot.centerYAnchor.constraint(equalTo: label.centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 6),
            dot.heightAnchor.constraint(equalToConstant: 6),
            label.leadingAnchor.constraint(equalTo: dot.trailingAnchor, constant: 6),
            label.topAnchor.constraint(equalTo: topAnchor, constant: 8),
            label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -4),
        ])

        let pulse = CABasicAnimation(keyPath: "opacity")
        pulse.fromValue = 0.25
        pulse.toValue = 1.0
        pulse.duration = 0.9
        pulse.autoreverses = true
        pulse.repeatCount = .infinity
        dot.layer?.add(pulse, forKey: "pulse")
    }

    required init?(coder: NSCoder) { fatalError() }
}

// MARK: - 侧边栏会话行

final class SessionCellView: NSTableCellView {
    let previewLabel = NSTextField(labelWithString: "")
    let metaLabel = NSTextField(labelWithString: "")

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        previewLabel.font = .systemFont(ofSize: 12)
        previewLabel.lineBreakMode = .byTruncatingTail
        previewLabel.translatesAutoresizingMaskIntoConstraints = false
        metaLabel.font = .systemFont(ofSize: 9.5)
        metaLabel.textColor = .tertiaryLabelColor
        metaLabel.translatesAutoresizingMaskIntoConstraints = false
        addSubview(previewLabel)
        addSubview(metaLabel)
        NSLayoutConstraint.activate([
            previewLabel.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            previewLabel.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            previewLabel.topAnchor.constraint(equalTo: topAnchor, constant: 8),
            metaLabel.leadingAnchor.constraint(equalTo: previewLabel.leadingAnchor),
            metaLabel.trailingAnchor.constraint(equalTo: previewLabel.trailingAnchor),
            metaLabel.topAnchor.constraint(equalTo: previewLabel.bottomAnchor, constant: 3),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }
}

// MARK: - 应用

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTextViewDelegate,
    NSTableViewDataSource, NSTableViewDelegate
{
    private var window: NSWindow!
    private let service = ChatService()

    private var scrollView: NSScrollView!
    private var document: FlippedView!
    private var stack: NSStackView!
    private var emptyState: NSStackView!
    private var statusLabel = NSTextField(labelWithString: "连接中…")
    private var modelLabel = NSTextField(labelWithString: "")

    private var sidebarTable: NSTableView!
    private var suppressSelection = false
    private var newChatButton: NSButton!
    private var effortPopup: NSPopUpButton!

    private var composerScroll: NSScrollView!
    private var composerTextView: NSTextView!
    private var composerPlaceholder = NSTextField(labelWithString: "说点什么…")
    private var composerHeight: NSLayoutConstraint!
    private var sendButton = NSButton()

    private var approvalCard: NSView!
    private var approvalTitle = NSTextField(labelWithString: "需要你确认")
    private var approvalDetail = NSTextField(labelWithString: "")
    private var approvalPreview = NSTextField(wrappingLabelWithString: "")

    private var rows: [MessageRowView] = []
    private var renderedIDs: [UUID] = []
    private var lastRenderedDate: Date?
    private var typingRow: TypingRowView?
    private var cancellables = Set<AnyCancellable>()

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.applicationIconImage = Self.makeIcon()
        buildMenu()
        buildWindow()
        bind()
        service.start()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window.makeFirstResponder(composerTextView)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
    func applicationWillTerminate(_ notification: Notification) { service.stop() }

    // MARK: 菜单

    private func buildMenu() {
        let mainMenu = NSMenu()
        let appItem = NSMenuItem()
        mainMenu.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        appMenu.addItem(withTitle: "关于 云熙", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "新会话", action: #selector(newChat), keyEquivalent: "n")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "退出 云熙", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        NSApp.mainMenu = mainMenu
    }

    // MARK: 窗口

    private func buildWindow() {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 760, height: 780),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙"
        window.minSize = NSSize(width: 680, height: 620)
        window.center()
        window.delegate = self
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("YunXiChatWindow")

        let background = NSVisualEffectView()
        background.material = .underWindowBackground
        background.blendingMode = .behindWindow
        background.state = .active
        window.contentView = background

        // —— 侧边栏 ——
        let sidebar = NSView()
        sidebar.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(sidebar)

        let sidebarTitle = NSTextField(labelWithString: "对话")
        sidebarTitle.font = .systemFont(ofSize: 11, weight: .semibold)
        sidebarTitle.textColor = .secondaryLabelColor
        sidebarTitle.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(sidebarTitle)

        sidebarTable = NSTableView()
        sidebarTable.headerView = nil
        sidebarTable.rowHeight = 52
        sidebarTable.backgroundColor = .clear
        sidebarTable.selectionHighlightStyle = .regular
        sidebarTable.allowsEmptySelection = true
        sidebarTable.dataSource = self
        sidebarTable.delegate = self
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("session"))
        column.width = 190
        sidebarTable.addTableColumn(column)

        let sidebarScroll = NSScrollView()
        sidebarScroll.documentView = sidebarTable
        sidebarScroll.hasVerticalScroller = true
        sidebarScroll.autohidesScrollers = true
        sidebarScroll.drawsBackground = false
        sidebarScroll.borderType = .noBorder
        sidebarScroll.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(sidebarScroll)

        let sidebarDivider = NSBox()
        sidebarDivider.boxType = .separator
        sidebarDivider.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(sidebarDivider)

        // —— 主列 ——
        let mainColumn = NSView()
        mainColumn.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(mainColumn)

        let avatar = AvatarView(diameter: 28, fontSize: 12)
        let name = NSTextField(labelWithString: "云熙")
        name.font = .systemFont(ofSize: 15, weight: .semibold)
        statusLabel.font = .systemFont(ofSize: 11)
        statusLabel.textColor = .secondaryLabelColor
        modelLabel.font = .systemFont(ofSize: 9.5)
        modelLabel.textColor = .tertiaryLabelColor

        let spacer = NSView()
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)

        effortPopup = NSPopUpButton(frame: .zero, pullsDown: false)
        effortPopup.addItems(withTitles: ["自动思考", "始终思考", "不思考"])
        effortPopup.controlSize = .small
        effortPopup.font = .systemFont(ofSize: 10)
        effortPopup.target = self
        effortPopup.action = #selector(effortChanged)
        effortPopup.translatesAutoresizingMaskIntoConstraints = false

        newChatButton = NSButton(title: "新会话", target: self, action: #selector(newChat))
        newChatButton.bezelStyle = .inline
        newChatButton.font = .systemFont(ofSize: 11)
        newChatButton.controlSize = .small

        let header = NSStackView(views: [avatar, name, statusLabel, spacer, modelLabel, effortPopup, newChatButton])
        header.orientation = .horizontal
        header.spacing = 8
        header.edgeInsets = NSEdgeInsets(top: 10, left: 16, bottom: 10, right: 16)
        header.translatesAutoresizingMaskIntoConstraints = false
        header.heightAnchor.constraint(equalToConstant: 48).isActive = true

        let divider = NSBox()
        divider.boxType = .separator
        divider.translatesAutoresizingMaskIntoConstraints = false

        scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.translatesAutoresizingMaskIntoConstraints = false

        document = FlippedView()
        document.translatesAutoresizingMaskIntoConstraints = false
        scrollView.documentView = document

        stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 0
        stack.edgeInsets = NSEdgeInsets(top: 12, left: 0, bottom: 12, right: 0)
        stack.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(stack)

        // 空态
        let bigAvatar = AvatarView(diameter: 64, fontSize: 26)
        let emptyTitle = NSTextField(labelWithString: "云熙")
        emptyTitle.font = .systemFont(ofSize: 20, weight: .semibold)
        let emptyHint = NSTextField(labelWithString: "说点什么吧，我在。")
        emptyHint.font = .systemFont(ofSize: 12)
        emptyHint.textColor = .secondaryLabelColor
        emptyState = NSStackView(views: [bigAvatar, emptyTitle, emptyHint])
        emptyState.orientation = .vertical
        emptyState.alignment = .centerX
        emptyState.spacing = 8
        emptyState.translatesAutoresizingMaskIntoConstraints = false
        mainColumn.addSubview(emptyState)

        // 审批卡片（悬浮在输入区上方）
        approvalCard = NSView()
        approvalCard.wantsLayer = true
        approvalCard.layer?.cornerRadius = 14
        approvalCard.layer?.backgroundColor = NSColor.cardSurface.cgColor
        approvalCard.layer?.borderWidth = 1
        approvalCard.layer?.borderColor = NSColor.bubbleBorder.cgColor
        approvalCard.layer?.shadowColor = NSColor.black.cgColor
        approvalCard.layer?.shadowOpacity = 0.08
        approvalCard.layer?.shadowRadius = 10
        approvalCard.layer?.shadowOffset = CGSize(width: 0, height: 3)
        approvalCard.isHidden = true
        approvalCard.translatesAutoresizingMaskIntoConstraints = false
        mainColumn.addSubview(approvalCard)

        approvalTitle.font = .systemFont(ofSize: 12, weight: .semibold)
        approvalDetail.font = .systemFont(ofSize: 10.5)
        approvalDetail.textColor = .secondaryLabelColor
        approvalDetail.lineBreakMode = .byTruncatingTail
        approvalPreview.font = .monospacedSystemFont(ofSize: 10.5, weight: .regular)
        approvalPreview.textColor = .secondaryLabelColor
        approvalPreview.maximumNumberOfLines = 6
        approvalPreview.lineBreakMode = .byTruncatingTail

        let approvalAllow = NSButton(title: "允许", target: self, action: #selector(approveAction))
        approvalAllow.bezelStyle = .rounded
        approvalAllow.controlSize = .small
        let approvalDeny = NSButton(title: "拒绝", target: self, action: #selector(denyAction))
        approvalDeny.bezelStyle = .rounded
        approvalDeny.controlSize = .small

        let approvalButtons = NSStackView(views: [approvalDeny, approvalAllow])
        approvalButtons.orientation = .horizontal
        approvalButtons.spacing = 6
        approvalButtons.alignment = .trailing
        approvalButtons.translatesAutoresizingMaskIntoConstraints = false

        let approvalContent = NSStackView(views: [approvalTitle, approvalDetail, approvalPreview, approvalButtons])
        approvalContent.orientation = .vertical
        approvalContent.alignment = .leading
        approvalContent.spacing = 5
        approvalContent.translatesAutoresizingMaskIntoConstraints = false
        approvalCard.addSubview(approvalContent)

        // 输入区
        let inputContainer = NSView()
        inputContainer.wantsLayer = true
        inputContainer.layer?.cornerRadius = 22
        inputContainer.layer?.backgroundColor = NSColor.composerSurface.cgColor
        inputContainer.layer?.borderWidth = 1
        inputContainer.layer?.borderColor = NSColor.bubbleBorder.cgColor
        inputContainer.layer?.shadowColor = NSColor.black.cgColor
        inputContainer.layer?.shadowOpacity = 0.06
        inputContainer.layer?.shadowRadius = 8
        inputContainer.layer?.shadowOffset = CGSize(width: 0, height: 2)
        inputContainer.translatesAutoresizingMaskIntoConstraints = false

        composerScroll = NSScrollView()
        composerScroll.hasVerticalScroller = false
        composerScroll.drawsBackground = false
        composerScroll.borderType = .noBorder
        composerScroll.translatesAutoresizingMaskIntoConstraints = false

        composerTextView = NSTextView()
        composerTextView.isRichText = false
        composerTextView.isEditable = true
        composerTextView.isSelectable = true
        composerTextView.drawsBackground = false
        composerTextView.font = .systemFont(ofSize: 14)
        composerTextView.textColor = .labelColor
        composerTextView.insertionPointColor = .yunxiAccent
        composerTextView.textContainerInset = NSSize(width: 0, height: 6)
        composerTextView.isVerticallyResizable = true
        composerTextView.isHorizontallyResizable = false
        composerTextView.autoresizingMask = [.width]
        composerTextView.textContainer?.widthTracksTextView = true
        composerTextView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        composerTextView.delegate = self
        composerScroll.documentView = composerTextView

        composerPlaceholder.font = .systemFont(ofSize: 14)
        composerPlaceholder.textColor = .placeholderTextColor
        composerPlaceholder.translatesAutoresizingMaskIntoConstraints = false

        sendButton = NSButton(
            image: NSImage(systemSymbolName: "arrow.up.circle.fill", accessibilityDescription: "发送") ?? NSImage(),
            target: self,
            action: #selector(sendMessage)
        )
        sendButton.isBordered = false
        sendButton.contentTintColor = .yunxiAccent
        sendButton.imageScaling = .scaleProportionallyUpOrDown
        sendButton.translatesAutoresizingMaskIntoConstraints = false

        inputContainer.addSubview(composerScroll)
        inputContainer.addSubview(composerPlaceholder)
        inputContainer.addSubview(sendButton)

        let hint = NSTextField(labelWithString: "仅本机 · Enter 发送、Shift+Enter 换行")
        hint.font = .systemFont(ofSize: 9.5)
        hint.textColor = .tertiaryLabelColor
        hint.translatesAutoresizingMaskIntoConstraints = false

        mainColumn.addSubview(header)
        mainColumn.addSubview(divider)
        mainColumn.addSubview(scrollView)
        mainColumn.addSubview(inputContainer)
        mainColumn.addSubview(hint)

        composerHeight = inputContainer.heightAnchor.constraint(equalToConstant: 44)
        composerHeight.isActive = true

        NSLayoutConstraint.activate([
            sidebar.leadingAnchor.constraint(equalTo: background.leadingAnchor),
            sidebar.topAnchor.constraint(equalTo: background.topAnchor),
            sidebar.bottomAnchor.constraint(equalTo: background.bottomAnchor),
            sidebar.widthAnchor.constraint(equalToConstant: 200),

            sidebarTitle.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 16),
            sidebarTitle.topAnchor.constraint(equalTo: sidebar.topAnchor, constant: 14),

            sidebarScroll.topAnchor.constraint(equalTo: sidebarTitle.bottomAnchor, constant: 8),
            sidebarScroll.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 6),
            sidebarScroll.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -6),
            sidebarScroll.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor, constant: -8),

            sidebarDivider.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            sidebarDivider.topAnchor.constraint(equalTo: background.topAnchor),
            sidebarDivider.bottomAnchor.constraint(equalTo: background.bottomAnchor),
            sidebarDivider.widthAnchor.constraint(equalToConstant: 1),

            mainColumn.leadingAnchor.constraint(equalTo: sidebarDivider.trailingAnchor),
            mainColumn.trailingAnchor.constraint(equalTo: background.trailingAnchor),
            mainColumn.topAnchor.constraint(equalTo: background.topAnchor),
            mainColumn.bottomAnchor.constraint(equalTo: background.bottomAnchor),

            header.topAnchor.constraint(equalTo: mainColumn.topAnchor),
            header.leadingAnchor.constraint(equalTo: mainColumn.leadingAnchor),
            header.trailingAnchor.constraint(equalTo: mainColumn.trailingAnchor),

            divider.topAnchor.constraint(equalTo: header.bottomAnchor),
            divider.leadingAnchor.constraint(equalTo: mainColumn.leadingAnchor),
            divider.trailingAnchor.constraint(equalTo: mainColumn.trailingAnchor),

            scrollView.topAnchor.constraint(equalTo: divider.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: mainColumn.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: mainColumn.trailingAnchor),

            stack.topAnchor.constraint(equalTo: document.topAnchor),
            stack.bottomAnchor.constraint(equalTo: document.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: document.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: document.trailingAnchor),
            document.widthAnchor.constraint(equalTo: scrollView.contentView.widthAnchor),
            document.heightAnchor.constraint(greaterThanOrEqualTo: scrollView.contentView.heightAnchor),

            emptyState.centerXAnchor.constraint(equalTo: scrollView.centerXAnchor),
            emptyState.centerYAnchor.constraint(equalTo: scrollView.centerYAnchor),

            inputContainer.topAnchor.constraint(equalTo: scrollView.bottomAnchor, constant: 8),
            inputContainer.leadingAnchor.constraint(equalTo: mainColumn.leadingAnchor, constant: 16),
            inputContainer.trailingAnchor.constraint(equalTo: mainColumn.trailingAnchor, constant: -16),

            composerScroll.leadingAnchor.constraint(equalTo: inputContainer.leadingAnchor, constant: 14),
            composerScroll.trailingAnchor.constraint(equalTo: sendButton.leadingAnchor, constant: -8),
            composerScroll.topAnchor.constraint(equalTo: inputContainer.topAnchor, constant: 7),
            composerScroll.bottomAnchor.constraint(equalTo: inputContainer.bottomAnchor, constant: -7),

            composerPlaceholder.leadingAnchor.constraint(equalTo: inputContainer.leadingAnchor, constant: 15),
            composerPlaceholder.centerYAnchor.constraint(equalTo: inputContainer.centerYAnchor),

            sendButton.trailingAnchor.constraint(equalTo: inputContainer.trailingAnchor, constant: -10),
            sendButton.bottomAnchor.constraint(equalTo: inputContainer.bottomAnchor, constant: -8),
            sendButton.widthAnchor.constraint(equalToConstant: 28),
            sendButton.heightAnchor.constraint(equalToConstant: 28),

            hint.topAnchor.constraint(equalTo: inputContainer.bottomAnchor, constant: 6),
            hint.leadingAnchor.constraint(equalTo: mainColumn.leadingAnchor, constant: 18),
            hint.bottomAnchor.constraint(equalTo: mainColumn.bottomAnchor, constant: -8),

            approvalCard.leadingAnchor.constraint(equalTo: mainColumn.leadingAnchor, constant: 16),
            approvalCard.trailingAnchor.constraint(equalTo: mainColumn.trailingAnchor, constant: -16),
            approvalCard.bottomAnchor.constraint(equalTo: inputContainer.topAnchor, constant: -8),

            approvalContent.topAnchor.constraint(equalTo: approvalCard.topAnchor, constant: 10),
            approvalContent.leadingAnchor.constraint(equalTo: approvalCard.leadingAnchor, constant: 12),
            approvalContent.trailingAnchor.constraint(equalTo: approvalCard.trailingAnchor, constant: -12),
            approvalContent.bottomAnchor.constraint(equalTo: approvalCard.bottomAnchor, constant: -10),
        ])
    }

    // MARK: 绑定

    private func bind() {
        service.$messages
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.render() }
            .store(in: &cancellables)
        service.$busy
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.render() }
            .store(in: &cancellables)
        service.$status
            .receive(on: RunLoop.main)
            .sink { [weak self] value in self?.statusLabel.stringValue = value }
            .store(in: &cancellables)
        service.$modelInfo
            .receive(on: RunLoop.main)
            .sink { [weak self] value in self?.modelLabel.stringValue = value }
            .store(in: &cancellables)
        service.$sessions
            .receive(on: RunLoop.main)
            .sink { [weak self] sessions in self?.reloadSessions(sessions) }
            .store(in: &cancellables)
        service.$pendingApproval
            .receive(on: RunLoop.main)
            .sink { [weak self] approval in self?.updateApproval(approval) }
            .store(in: &cancellables)
        service.$effort
            .receive(on: RunLoop.main)
            .sink { [weak self] effort in
                let index = ["auto": 0, "on": 1, "off": 2][effort] ?? 0
                self?.effortPopup.selectItem(at: index)
            }
            .store(in: &cancellables)
        service.$connected
            .receive(on: RunLoop.main)
            .sink { [weak self] connected in
                self?.composerTextView.isEditable = connected
                self?.sendButton.isEnabled = connected
                self?.sidebarTable.isEnabled = connected
                self?.effortPopup.isEnabled = connected
                self?.newChatButton.isEnabled = connected
            }
            .store(in: &cancellables)
        service.$busy
            .receive(on: RunLoop.main)
            .sink { [weak self] busy in
                self?.sidebarTable.isEnabled = self?.service.connected == true && !busy
                self?.newChatButton.isEnabled = self?.service.connected == true && !busy
            }
            .store(in: &cancellables)
    }

    // MARK: 渲染

    private func render() {
        let messages = service.messages
        let ids = messages.map(\.id)

        if ids.count < renderedIDs.count || !ids.starts(with: renderedIDs) {
            rebuild()
        } else {
            if ids.count > renderedIDs.count {
                for index in renderedIDs.count..<messages.count {
                    appendRow(for: messages[index])
                }
                renderedIDs = ids
            }
            for (index, row) in rows.enumerated() where index < messages.count {
                row.update(messages[index])
            }
        }

        emptyState.isHidden = !messages.isEmpty
        updateTyping(visible: service.busy)
        scrollToBottom()
    }

    private func rebuild() {
        for row in rows { row.removeFromSuperview() }
        rows.removeAll()
        if let typingRow { typingRow.removeFromSuperview(); self.typingRow = nil }
        lastRenderedDate = nil
        for message in service.messages {
            appendRow(for: message)
        }
        renderedIDs = service.messages.map(\.id)
    }

    private func appendRow(for message: ChatMessage) {
        let showDate: Bool
        if let at = message.at {
            if let last = lastRenderedDate {
                showDate = !Calendar.current.isDate(last, inSameDayAs: at)
            } else {
                showDate = true
            }
            lastRenderedDate = at
        } else {
            showDate = false
        }
        let row = MessageRowView(message: message, showDate: showDate)
        stack.addArrangedSubview(row)
        row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        rows.append(row)
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
            let maxY = max(0, height - visible + 24)
            self.scrollView.contentView.scroll(to: NSPoint(x: 0, y: maxY))
            self.scrollView.reflectScrolledClipView(self.scrollView.contentView)
        }
    }

    // MARK: 侧边栏

    private func reloadSessions(_ sessions: [ChatSession]) {
        suppressSelection = true
        sidebarTable.reloadData()
        if let index = sessions.firstIndex(where: { $0.id == service.session }) {
            sidebarTable.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false)
        }
        suppressSelection = false
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        service.sessions.count
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row < service.sessions.count else { return nil }
        let item = service.sessions[row]
        let identifier = NSUserInterfaceItemIdentifier("session-cell")
        let cell = (tableView.makeView(withIdentifier: identifier, owner: self) as? SessionCellView) ?? SessionCellView()
        cell.identifier = identifier
        cell.previewLabel.stringValue = item.preview.isEmpty ? item.id : item.preview
        cell.metaLabel.stringValue = "\(item.turns) 轮"
        return cell
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        guard !suppressSelection else { return }
        let row = sidebarTable.selectedRow
        guard row >= 0, row < service.sessions.count else { return }
        service.openSession(service.sessions[row].id)
    }

    // MARK: 审批卡片

    private func updateApproval(_ approval: PendingApproval?) {
        guard let approval else {
            approvalCard.isHidden = true
            return
        }
        approvalTitle.stringValue = "需要你确认 · \(approval.tool)（\(approval.capability)）"
        var detail = approval.reason
        if let specifier = approval.specifier, !specifier.isEmpty {
            detail += " · 对象：\(specifier)"
        }
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

    @objc private func effortChanged() {
        let values = ["auto", "on", "off"]
        let index = effortPopup.indexOfSelectedItem
        if index >= 0 && index < values.count {
            service.setEffort(values[index])
        }
    }

    // MARK: 输入

    func textDidChange(_ notification: Notification) {
        let text = composerTextView.string
        composerPlaceholder.isHidden = !text.isEmpty
        let used = composerTextView.layoutManager?.usedRect(for: composerTextView.textContainer!) ?? .zero
        let height = min(112, max(44, used.height + 20))
        if abs(composerHeight.constant - height) > 0.5 {
            composerHeight.constant = height
            window.contentView?.layoutSubtreeIfNeeded()
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
        composerHeight.constant = 44
        sendButton.isEnabled = false
        service.send(text)
        window.makeFirstResponder(composerTextView)
    }

    @objc private func newChat() {
        service.newChat()
    }

    // MARK: 图标

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
        text.draw(
            at: NSPoint(x: (size.width - textSize.width) / 2, y: (size.height - textSize.height) / 2 - 6),
            withAttributes: attrs
        )
        return image
    }
}
