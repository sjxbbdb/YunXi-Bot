import AppKit
import Combine

// MARK: - 配色

extension NSColor {
    static let yunxiAccent = NSColor(calibratedRed: 0.66, green: 0.33, blue: 0.42, alpha: 1.0)

    /// 亮/暗两套色，跟随系统外观。
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
        light: NSColor(calibratedWhite: 0.945, alpha: 1.0),
        dark: NSColor(calibratedWhite: 0.19, alpha: 1.0)
    )
    static let bubbleBorder = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.86, alpha: 1.0),
        dark: NSColor(calibratedWhite: 0.28, alpha: 1.0)
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

// MARK: - 气泡与消息行

final class BubbleView: NSView {
    let label = NSTextField(wrappingLabelWithString: "")

    init(isUser: Bool) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 15
        layer?.backgroundColor = (isUser ? NSColor.bubbleUser : NSColor.bubbleAssistant).cgColor
        layer?.borderWidth = isUser ? 0 : 0.5
        layer?.borderColor = NSColor.bubbleBorder.cgColor

        label.font = .systemFont(ofSize: 14)
        label.textColor = isUser ? .white : .labelColor
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
    }

    required init?(coder: NSCoder) { fatalError() }

    func updateText(_ text: String) {
        if label.stringValue != text { label.stringValue = text }
    }
}

final class MessageRowView: NSView {
    let bubble: BubbleView?
    let textLabel: NSTextField?
    private let role: ChatMessage.Role

    private static let timeFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()

    init(message: ChatMessage) {
        role = message.role
        if message.role == .system {
            bubble = nil
            textLabel = NSTextField(wrappingLabelWithString: message.text)
        } else {
            bubble = BubbleView(isUser: message.role == .user)
            textLabel = nil
            bubble?.updateText(message.text)
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
        let time = NSTextField(labelWithString: message.at.map { Self.timeFormatter.string(from: $0) } ?? "")
        time.font = .systemFont(ofSize: 9.5)
        time.textColor = .tertiaryLabelColor
        time.isHidden = message.at == nil
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
            bubble.updateText(message.text)
        } else if let textLabel {
            if textLabel.stringValue != message.text { textLabel.stringValue = message.text }
        }
    }
}

// MARK: - 应用

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var window: NSWindow!
    private let service = ChatService()

    private var scrollView: NSScrollView!
    private var document: FlippedView!
    private var stack: NSStackView!
    private var emptyState: NSStackView!
    private var statusLabel = NSTextField(labelWithString: "连接中…")
    private var modelLabel = NSTextField(labelWithString: "")
    private var input = NSTextField()
    private var sendButton = NSButton()
    private var rows: [MessageRowView] = []
    private var renderedIDs: [UUID] = []
    private var typingRow: NSView?
    private var cancellables = Set<AnyCancellable>()

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.applicationIconImage = Self.makeIcon()
        buildMenu()
        buildWindow()
        bind()
        service.start()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
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
            contentRect: NSRect(x: 0, y: 0, width: 500, height: 740),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙"
        window.minSize = NSSize(width: 440, height: 600)
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

        // 输入区
        let inputContainer = NSView()
        inputContainer.wantsLayer = true
        inputContainer.layer?.cornerRadius = 16
        inputContainer.layer?.backgroundColor = NSColor.adaptive(
            light: NSColor(calibratedWhite: 0.985, alpha: 1.0),
            dark: NSColor(calibratedWhite: 0.16, alpha: 1.0)
        ).cgColor
        inputContainer.layer?.borderWidth = 0.5
        inputContainer.layer?.borderColor = NSColor.bubbleBorder.cgColor
        inputContainer.translatesAutoresizingMaskIntoConstraints = false

        input.placeholderString = "说点什么…"
        input.font = .systemFont(ofSize: 14)
        input.isBordered = false
        input.drawsBackground = false
        input.focusRingType = .none
        input.target = self
        input.action = #selector(sendMessage)
        input.translatesAutoresizingMaskIntoConstraints = false

        sendButton = NSButton(image: NSImage(systemSymbolName: "arrow.up.circle.fill", accessibilityDescription: "发送") ?? NSImage(), target: self, action: #selector(sendMessage))
        sendButton.isBordered = false
        sendButton.contentTintColor = .yunxiAccent
        sendButton.imageScaling = .scaleProportionallyUpOrDown
        sendButton.translatesAutoresizingMaskIntoConstraints = false

        inputContainer.addSubview(input)
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
            inputContainer.heightAnchor.constraint(equalToConstant: 44),

            input.leadingAnchor.constraint(equalTo: inputContainer.leadingAnchor, constant: 14),
            input.centerYAnchor.constraint(equalTo: inputContainer.centerYAnchor),
            input.trailingAnchor.constraint(equalTo: sendButton.leadingAnchor, constant: -8),

            sendButton.trailingAnchor.constraint(equalTo: inputContainer.trailingAnchor, constant: -8),
            sendButton.centerYAnchor.constraint(equalTo: inputContainer.centerYAnchor),
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
                self?.input.isEnabled = connected
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
                    appendRow(MessageRowView(message: messages[index]))
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
        for message in service.messages {
            appendRow(MessageRowView(message: message))
        }
        renderedIDs = service.messages.map(\.id)
    }

    private func appendRow(_ row: MessageRowView) {
        stack.addArrangedSubview(row)
        row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        rows.append(row)
    }

    private func updateTyping(visible: Bool) {
        if visible, typingRow == nil {
            let label = NSTextField(labelWithString: "云熙正在想…")
            label.font = .systemFont(ofSize: 11)
            label.textColor = .secondaryLabelColor
            let holder = NSView()
            holder.translatesAutoresizingMaskIntoConstraints = false
            label.translatesAutoresizingMaskIntoConstraints = false
            holder.addSubview(label)
            NSLayoutConstraint.activate([
                label.leadingAnchor.constraint(equalTo: holder.leadingAnchor, constant: 48),
                label.topAnchor.constraint(equalTo: holder.topAnchor, constant: 8),
                label.bottomAnchor.constraint(equalTo: holder.bottomAnchor, constant: -4),
            ])
            stack.addArrangedSubview(holder)
            holder.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
            typingRow = holder
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

    // MARK: 动作

    @objc private func sendMessage() {
        let text = input.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        input.stringValue = ""
        service.send(text)
        window.makeFirstResponder(input)
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
