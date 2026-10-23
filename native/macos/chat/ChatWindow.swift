import AppKit
import Combine

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var window: NSWindow!
    private let service = ChatService()
    private let transcript = NSTextView()
    private let statusLabel = NSTextField(labelWithString: "连接中…")
    private let modelLabel = NSTextField(labelWithString: "")
    private let input = NSTextField()
    private let sendButton = NSButton(title: "发送", target: nil, action: nil)
    private var cancellables = Set<AnyCancellable>()

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenu()
        buildWindow()
        bind()
        service.start()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        service.stop()
    }

    // MARK: - 界面

    private func buildMenu() {
        let mainMenu = NSMenu()
        let appItem = NSMenuItem()
        mainMenu.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        appMenu.addItem(withTitle: "关于 云熙", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "退出 云熙", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        NSApp.mainMenu = mainMenu
    }

    private func buildWindow() {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 720),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙"
        window.minSize = NSSize(width: 420, height: 560)
        window.center()
        window.delegate = self
        window.isReleasedWhenClosed = false

        let content = NSView()
        window.contentView = content

        // 顶栏：状态点 + 名字 + 状态 + 模型 + 新会话
        let dot = NSView()
        dot.wantsLayer = true
        dot.layer?.backgroundColor = NSColor.systemOrange.cgColor
        dot.layer?.cornerRadius = 4
        dot.translatesAutoresizingMaskIntoConstraints = false
        dot.widthAnchor.constraint(equalToConstant: 8).isActive = true
        dot.heightAnchor.constraint(equalToConstant: 8).isActive = true

        let name = NSTextField(labelWithString: "云熙")
        name.font = NSFont.systemFont(ofSize: 15, weight: .semibold)
        statusLabel.textColor = NSColor.secondaryLabelColor
        statusLabel.font = NSFont.systemFont(ofSize: 11)
        modelLabel.textColor = NSColor.tertiaryLabelColor
        modelLabel.font = NSFont.systemFont(ofSize: 10)

        let spacer = NSView()
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)

        let newChat = NSButton(title: "新会话", target: self, action: #selector(newChat))
        newChat.bezelStyle = .inline
        newChat.font = NSFont.systemFont(ofSize: 11)
        newChat.controlSize = .small

        let header = NSStackView(views: [dot, name, statusLabel, spacer, modelLabel, newChat])
        header.orientation = .horizontal
        header.spacing = 8
        header.edgeInsets = NSEdgeInsets(top: 10, left: 14, bottom: 10, right: 14)
        header.translatesAutoresizingMaskIntoConstraints = false
        header.heightAnchor.constraint(equalToConstant: 38).isActive = true

        // 对话区
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        scroll.drawsBackground = false
        scroll.translatesAutoresizingMaskIntoConstraints = false

        transcript.isEditable = false
        transcript.isSelectable = true
        transcript.drawsBackground = false
        transcript.textContainerInset = NSSize(width: 14, height: 12)
        transcript.font = NSFont.systemFont(ofSize: 14)
        transcript.isVerticallyResizable = true
        transcript.isHorizontallyResizable = false
        transcript.autoresizingMask = [.width]
        transcript.textContainer?.widthTracksTextView = true
        transcript.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        scroll.documentView = transcript

        // 输入区
        input.placeholderString = "说点什么…"
        input.font = NSFont.systemFont(ofSize: 14)
        input.bezelStyle = .roundedBezel
        input.target = self
        input.action = #selector(sendMessage)
        input.translatesAutoresizingMaskIntoConstraints = false

        sendButton.target = self
        sendButton.action = #selector(sendMessage)
        sendButton.bezelStyle = .rounded
        sendButton.keyEquivalent = "\r"
        sendButton.translatesAutoresizingMaskIntoConstraints = false

        let hint = NSTextField(labelWithString: "需要人工批准的动作会被拒绝；可在终端 `chat` 里处理。")
        hint.font = NSFont.systemFont(ofSize: 10)
        hint.textColor = NSColor.tertiaryLabelColor

        let inputRow = NSStackView(views: [input, sendButton])
        inputRow.orientation = .horizontal
        inputRow.spacing = 8
        inputRow.translatesAutoresizingMaskIntoConstraints = false

        let composer = NSStackView(views: [inputRow, hint])
        composer.orientation = .vertical
        composer.alignment = .leading
        composer.spacing = 6
        composer.edgeInsets = NSEdgeInsets(top: 10, left: 14, bottom: 10, right: 14)
        composer.translatesAutoresizingMaskIntoConstraints = false

        content.addSubview(header)
        content.addSubview(scroll)
        content.addSubview(composer)

        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: content.topAnchor),
            header.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            header.trailingAnchor.constraint(equalTo: content.trailingAnchor),

            scroll.topAnchor.constraint(equalTo: header.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: content.trailingAnchor),

            composer.topAnchor.constraint(equalTo: scroll.bottomAnchor),
            composer.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            composer.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            composer.bottomAnchor.constraint(equalTo: content.bottomAnchor),

            inputRow.widthAnchor.constraint(equalTo: composer.widthAnchor, constant: -28),
            input.widthAnchor.constraint(greaterThanOrEqualToConstant: 200),
            sendButton.widthAnchor.constraint(equalToConstant: 64),
        ])
    }


    // MARK: - 绑定

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

    // MARK: - 渲染

    private func render() {
        let result = NSMutableAttributedString()
        for message in service.messages {
            result.append(paragraph(for: message))
        }
        if service.busy {
            let style = NSMutableParagraphStyle()
            style.paragraphSpacing = 8
            result.append(NSAttributedString(
                string: "云熙正在想…\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 12),
                    .foregroundColor: NSColor.secondaryLabelColor,
                    .paragraphStyle: style,
                ]
            ))
        }
        transcript.textStorage?.setAttributedString(result)
        let end = (transcript.string as NSString).length
        transcript.scrollRangeToVisible(NSRange(location: end, length: 0))
    }

    private func paragraph(for message: ChatMessage) -> NSAttributedString {
        let style = NSMutableParagraphStyle()
        style.paragraphSpacing = 10
        style.lineSpacing = 3

        let out = NSMutableAttributedString()
        switch message.role {
        case .system:
            out.append(NSAttributedString(
                string: "· \(message.text)\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 11),
                    .foregroundColor: NSColor.secondaryLabelColor,
                    .paragraphStyle: style,
                ]
            ))
        case .user:
            out.append(NSAttributedString(
                string: "你\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 10, weight: .semibold),
                    .foregroundColor: NSColor.secondaryLabelColor,
                    .paragraphStyle: style,
                ]
            ))
            out.append(NSAttributedString(
                string: message.text + "\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 14),
                    .foregroundColor: NSColor.labelColor,
                    .paragraphStyle: style,
                ]
            ))
        case .assistant:
            out.append(NSAttributedString(
                string: "云熙\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 10, weight: .semibold),
                    .foregroundColor: NSColor(calibratedRed: 0.55, green: 0.32, blue: 0.42, alpha: 1),
                    .paragraphStyle: style,
                ]
            ))
            out.append(NSAttributedString(
                string: message.text + "\n",
                attributes: [
                    .font: NSFont.systemFont(ofSize: 14),
                    .foregroundColor: NSColor.labelColor,
                    .paragraphStyle: style,
                ]
            ))
        }
        return out
    }

    // MARK: - 动作

    @objc private func sendMessage() {
        let text = input.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        input.stringValue = ""
        service.send(text)
    }

    @objc private func newChat() {
        service.newChat()
    }
}
