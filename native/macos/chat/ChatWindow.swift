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

// MARK: - 打字指示（参考 animate-token-pulsing-dot）

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

// MARK: - 应用

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTextViewDelegate {
    private var window: NSWindow!
    private let service = ChatService()

    private var scrollView: NSScrollView!
    private var document: FlippedView!
    private var stack: NSStackView!
    private var emptyState: NSStackView!
    private var statusLabel = NSTextField(labelWithString: "连接中…")
    private var modelLabel = NSTextField(labelWithString: "")

    private var composerScroll: NSScrollView!
    private var composerTextView: NSTextView!
    private var composerPlaceholder = NSTextField(labelWithString: "说点什么…")
    private var composerHeight: NSLayoutConstraint!
    private var sendButton = NSButton()

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
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 760),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙"
        window.minSize = NSSize(width: 460, height: 620)
        window.center()
        window.delegate = self
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("YunXiChatWindow")

        let background = NSVisualEffectView()
        background.material = .underWindowBackground
        background.blendingMode = .behindWindow
        background.state = .active
        window.contentView = background

        // 顶栏
        let avatar = AvatarView(diameter: 28, fontSize: 12)
        let name = NSTextField(labelWithString: "云熙")
        name.font = .systemFont(ofSize: 15, weight: .semibold)
        statusLabel.font = .systemFont(ofSize: 11)
        statusLabel.textColor = .secondaryLabelColor
        modelLabel.font = .systemFont(ofSize: 9.5)
        modelLabel.textColor = .tertiaryLabelColor

        let spacer = NSView()
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)

        let newChatButton = NSButton(title: "新会话", target: self, action: #selector(newChat))
        newChatButton.bezelStyle = .inline
        newChatButton.font = .systemFont(ofSize: 11)
        newChatButton.controlSize = .small

        let header = NSStackView(views: [avatar, name, statusLabel, spacer, modelLabel, newChatButton])
        header.orientation = .horizontal
        header.spacing = 8
        header.edgeInsets = NSEdgeInsets(top: 10, left: 16, bottom: 10, right: 16)
        header.translatesAutoresizingMaskIntoConstraints = false
        header.heightAnchor.constraint(equalToConstant: 48).isActive = true

        let divider = NSBox()
        divider.boxType = .separator
        divider.translatesAutoresizingMaskIntoConstraints = false

        // 对话区
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
        background.addSubview(emptyState)

        // 输入区（参考 Codex composer：最小 44、圆角 22、elevation）
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

        sendButton = NSButton(image: NSImage(systemSymbolName: "arrow.up.circle.fill", accessibilityDescription: "发送") ?? NSImage(), target: self, action: #selector(sendMessage))
        sendButton.isBordered = false
        sendButton.contentTintColor = .yunxiAccent
        sendButton.imageScaling = .scaleProportionallyUpOrDown
        sendButton.translatesAutoresizingMaskIntoConstraints = false

        inputContainer.addSubview(composerScroll)
        inputContainer.addSubview(composerPlaceholder)
        inputContainer.addSubview(sendButton)

        let hint = NSTextField(labelWithString: "仅本机 · 需要人工批准的动作会被拒绝，可在终端 chat 处理")
        hint.font = .systemFont(ofSize: 9.5)
        hint.textColor = .tertiaryLabelColor
        hint.translatesAutoresizingMaskIntoConstraints = false

        background.addSubview(header)
        background.addSubview(divider)
        background.addSubview(scrollView)
        background.addSubview(inputContainer)
        background.addSubview(hint)

        composerHeight = inputContainer.heightAnchor.constraint(equalToConstant: 44)
        composerHeight.isActive = true

        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: background.topAnchor),
            header.leadingAnchor.constraint(equalTo: background.leadingAnchor),
            header.trailingAnchor.constraint(equalTo: background.trailingAnchor),

            divider.topAnchor.constraint(equalTo: header.bottomAnchor),
            divider.leadingAnchor.constraint(equalTo: background.leadingAnchor),
            divider.trailingAnchor.constraint(equalTo: background.trailingAnchor),

            scrollView.topAnchor.constraint(equalTo: divider.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: background.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: background.trailingAnchor),

            stack.topAnchor.constraint(equalTo: document.topAnchor),
            stack.bottomAnchor.constraint(equalTo: document.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: document.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: document.trailingAnchor),
            document.widthAnchor.constraint(equalTo: scrollView.contentView.widthAnchor),
            document.heightAnchor.constraint(greaterThanOrEqualTo: scrollView.contentView.heightAnchor),

            emptyState.centerXAnchor.constraint(equalTo: scrollView.centerXAnchor),
            emptyState.centerYAnchor.constraint(equalTo: scrollView.centerYAnchor),

            inputContainer.topAnchor.constraint(equalTo: scrollView.bottomAnchor, constant: 8),
            inputContainer.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 16),
            inputContainer.trailingAnchor.constraint(equalTo: background.trailingAnchor, constant: -16),

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
            hint.leadingAnchor.constraint(equalTo: background.leadingAnchor, constant: 18),
            hint.bottomAnchor.constraint(equalTo: background.bottomAnchor, constant: -8),
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
        service.$connected
            .receive(on: RunLoop.main)
            .sink { [weak self] connected in
                self?.composerTextView.isEditable = connected
                self?.sendButton.isEnabled = connected
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
        guard !text.isEmpty, service.connected else { return }
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
