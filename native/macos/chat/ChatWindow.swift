import AppKit
import Combine

// MARK: - 主题（照 Codex 深色外壳取样：rail #1b1b1c / sidebar #131315 / main #0f0f11 / card #2a2a2c）

enum Theme {
    static let accent = NSColor.adaptive(
        light: NSColor(calibratedRed: 0.69, green: 0.34, blue: 0.43, alpha: 1),
        dark: NSColor(calibratedRed: 0.80, green: 0.45, blue: 0.53, alpha: 1)
    )
    static let window = NSColor.adaptive(
        light: NSColor(calibratedWhite: 1.0, alpha: 1),
        dark: NSColor(calibratedRed: 0.059, green: 0.059, blue: 0.067, alpha: 1)
    )
    static let rail = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.965, alpha: 1),
        dark: NSColor(calibratedRed: 0.106, green: 0.106, blue: 0.110, alpha: 1)
    )
    static let sidebar = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.980, alpha: 1),
        dark: NSColor(calibratedRed: 0.075, green: 0.075, blue: 0.082, alpha: 1)
    )
    static let elevated = NSColor.adaptive(
        light: NSColor(calibratedWhite: 1.0, alpha: 1),
        dark: NSColor(calibratedRed: 0.165, green: 0.165, blue: 0.173, alpha: 1)
    )
    static let chip = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.945, alpha: 1),
        dark: NSColor(calibratedRed: 0.118, green: 0.118, blue: 0.125, alpha: 1)
    )
    static let selection = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.918, alpha: 1),
        dark: NSColor(calibratedRed: 0.122, green: 0.122, blue: 0.129, alpha: 1)
    )
    static let hover = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.945, alpha: 1),
        dark: NSColor(calibratedRed: 0.137, green: 0.137, blue: 0.145, alpha: 1)
    )
    static let border = NSColor.adaptive(
        light: NSColor(calibratedWhite: 0.900, alpha: 1),
        dark: NSColor(calibratedRed: 0.173, green: 0.173, blue: 0.180, alpha: 1)
    )
    static let userBubble = NSColor.adaptive(
        light: NSColor(calibratedRed: 0.72, green: 0.36, blue: 0.45, alpha: 1),
        dark: NSColor(calibratedRed: 0.63, green: 0.30, blue: 0.38, alpha: 1)
    )

    static func applyEnvironmentAppearance() {
        let saved = UserDefaults.standard.string(forKey: "YunXiAppearance") ?? "dark"
        let env = ProcessInfo.processInfo.environment["YUNXI_APPEARANCE"]
        apply(name: env ?? saved)
    }

    static func apply(name: String) {
        switch name {
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        default: NSApp.appearance = nil
        }
    }
}

extension NSColor {
    static var yunxiAccent: NSColor { Theme.accent }

    static func adaptive(light: NSColor, dark: NSColor) -> NSColor {
        NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
        }
    }
}

/// 动态色块：外观切换时自动重刷 layer 颜色。
final class ColorView: NSView {
    var fill: NSColor? { didSet { refresh() } }
    var borderColor: NSColor? { didSet { refresh() } }
    var borderWidth: CGFloat = 0 { didSet { refresh() } }
    var cornerRadius: CGFloat = 0 { didSet { refresh() } }

    init(fill: NSColor? = nil, radius: CGFloat = 0, border: NSColor? = nil, borderWidth: CGFloat = 0) {
        self.fill = fill
        self.cornerRadius = radius
        self.borderColor = border
        self.borderWidth = borderWidth
        super.init(frame: .zero)
        wantsLayer = true
        translatesAutoresizingMaskIntoConstraints = false
        refresh()
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        refresh()
    }

    private func refresh() {
        guard let layer else { return }
        layer.cornerRadius = cornerRadius
        layer.cornerCurve = .continuous
        layer.backgroundColor = fill?.cgColor
        layer.borderWidth = borderWidth
        layer.borderColor = borderColor?.cgColor
    }
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
        layer?.cornerCurve = .continuous
        layer?.backgroundColor = Theme.accent.cgColor
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

private func symbol(_ name: String, size: CGFloat, weight: NSFont.Weight = .regular, tint: NSColor? = nil) -> NSImage? {
    let config = NSImage.SymbolConfiguration(pointSize: size, weight: weight)
    let image = NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(config)
    image?.isTemplate = true
    return image
}

private func iconButton(_ name: String, size: CGFloat = 16) -> NSButton {
    let button = NSButton()
    button.image = symbol(name, size: size)
    button.imageScaling = .scaleProportionallyUpOrDown
    button.isBordered = false
    button.bezelStyle = .regularSquare
    button.contentTintColor = .secondaryLabelColor
    button.translatesAutoresizingMaskIntoConstraints = false
    button.widthAnchor.constraint(equalToConstant: 26).isActive = true
    button.heightAnchor.constraint(equalToConstant: 26).isActive = true
    return button
}

// MARK: - 消息行

final class MessageRowView: NSView {
    private let role: ChatMessage.Role
    private let bubble: ColorView?
    private let label: NSTextField?

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
            label = NSTextField(wrappingLabelWithString: message.text)
        } else {
            bubble = ColorView(fill: message.role == .user ? Theme.userBubble : nil, radius: 18)
            label = NSTextField(wrappingLabelWithString: "")
        }
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false

        if role == .system {
            guard let label else { return }
            label.font = .systemFont(ofSize: 10.5)
            label.textColor = .tertiaryLabelColor
            label.alignment = .center
            label.translatesAutoresizingMaskIntoConstraints = false
            addSubview(label)
            NSLayoutConstraint.activate([
                label.topAnchor.constraint(equalTo: topAnchor, constant: 4),
                label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -4),
                label.centerXAnchor.constraint(equalTo: centerXAnchor),
                label.widthAnchor.constraint(lessThanOrEqualToConstant: 560),
            ])
            return
        }

        guard let bubble, let label else { return }
        label.isSelectable = true
        label.maximumNumberOfLines = 0
        label.translatesAutoresizingMaskIntoConstraints = false

        if role == .assistant {
            let avatar = AvatarView(diameter: 22, fontSize: 9.5)
            addSubview(avatar)
            addSubview(label)
            label.preferredMaxLayoutWidth = 640
            NSLayoutConstraint.activate([
                avatar.leadingAnchor.constraint(equalTo: leadingAnchor),
                avatar.topAnchor.constraint(equalTo: topAnchor, constant: 2),

                label.leadingAnchor.constraint(equalTo: avatar.trailingAnchor, constant: 10),
                label.topAnchor.constraint(equalTo: topAnchor, constant: 2),
                label.widthAnchor.constraint(lessThanOrEqualToConstant: 640),
                label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
            ])
            updateText(message.text)
            return
        }

        bubble.addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: bubble.leadingAnchor, constant: 13),
            label.trailingAnchor.constraint(equalTo: bubble.trailingAnchor, constant: -13),
            label.topAnchor.constraint(equalTo: bubble.topAnchor, constant: 9),
            label.bottomAnchor.constraint(equalTo: bubble.bottomAnchor, constant: -9),
        ])
        label.preferredMaxLayoutWidth = 520
        addSubview(bubble)
        NSLayoutConstraint.activate([
            bubble.topAnchor.constraint(equalTo: topAnchor, constant: 2),
            bubble.trailingAnchor.constraint(equalTo: trailingAnchor),
            bubble.widthAnchor.constraint(lessThanOrEqualToConstant: 560),
            bubble.leadingAnchor.constraint(greaterThanOrEqualTo: leadingAnchor, constant: 80),
            bubble.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
        ])
        updateText(message.text)
    }

    required init?(coder: NSCoder) { fatalError() }

    private func updateText(_ text: String) {
        label?.attributedStringValue = MarkdownRenderer.render(text, isUser: role == .user)
    }

    func update(_ message: ChatMessage) {
        updateText(message.text)
    }
}

// MARK: - 打字指示

final class TypingRowView: NSView {
    private let dot = NSView()
    private let label = NSTextField(labelWithString: "正在想…")

    init() {
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        let avatar = AvatarView(diameter: 22, fontSize: 9.5)
        addSubview(avatar)

        dot.wantsLayer = true
        dot.layer?.cornerRadius = 2.5
        dot.layer?.backgroundColor = Theme.accent.cgColor
        dot.translatesAutoresizingMaskIntoConstraints = false
        addSubview(dot)

        label.font = .systemFont(ofSize: 11)
        label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)

        NSLayoutConstraint.activate([
            avatar.leadingAnchor.constraint(equalTo: leadingAnchor),
            avatar.topAnchor.constraint(equalTo: topAnchor, constant: 2),
            dot.leadingAnchor.constraint(equalTo: avatar.trailingAnchor, constant: 12),
            dot.centerYAnchor.constraint(equalTo: label.centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 5),
            dot.heightAnchor.constraint(equalToConstant: 5),
            label.leadingAnchor.constraint(equalTo: dot.trailingAnchor, constant: 6),
            label.topAnchor.constraint(equalTo: topAnchor, constant: 7),
            label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
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

// MARK: - 会话行

final class SessionRowView: NSView {
    private let background = ColorView(fill: .clear, radius: 9)
    private let preview = NSTextField(labelWithString: "")
    private let meta = NSTextField(labelWithString: "")
    private let session: ChatSession
    private let onClick: () -> Void
    private var hovered = false
    private var selected = false
    private var tracking: NSTrackingArea?

    init(session: ChatSession, selected: Bool, onClick: @escaping () -> Void) {
        self.session = session
        self.onClick = onClick
        self.selected = selected
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        addSubview(background)

        if session.hasHistory {
            preview.stringValue = session.preview.isEmpty ? "（无预览）" : session.preview
            preview.textColor = .labelColor
        } else {
            preview.stringValue = "旧会话 · " + Self.dateString(session.updatedAt)
            preview.textColor = .secondaryLabelColor
        }
        preview.font = .systemFont(ofSize: 12.5)
        preview.lineBreakMode = .byTruncatingTail
        preview.translatesAutoresizingMaskIntoConstraints = false

        meta.stringValue = "\(session.turns) 轮 · " + Self.timeString(session.updatedAt)
        meta.font = .systemFont(ofSize: 9.5)
        meta.textColor = .tertiaryLabelColor
        meta.translatesAutoresizingMaskIntoConstraints = false

        addSubview(preview)
        addSubview(meta)
        NSLayoutConstraint.activate([
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.topAnchor.constraint(equalTo: topAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),

            preview.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            preview.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            preview.topAnchor.constraint(equalTo: topAnchor, constant: 7),

            meta.leadingAnchor.constraint(equalTo: preview.leadingAnchor),
            meta.topAnchor.constraint(equalTo: preview.bottomAnchor, constant: 2),
            meta.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
            heightAnchor.constraint(equalToConstant: 44),
        ])
        refreshBackground()
    }

    required init?(coder: NSCoder) { fatalError() }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }
        let area = NSTrackingArea(rect: bounds, options: [.mouseEnteredAndExited, .activeInKeyWindow], owner: self)
        addTrackingArea(area)
        tracking = area
    }

    override func mouseEntered(with event: NSEvent) {
        hovered = true
        refreshBackground()
    }

    override func mouseExited(with event: NSEvent) {
        hovered = false
        refreshBackground()
    }

    override func mouseDown(with event: NSEvent) {
        onClick()
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }

    private func refreshBackground() {
        background.fill = selected ? Theme.selection : (hovered ? Theme.hover : .clear)
    }

    private static func dateString(_ ms: Double) -> String {
        let f = DateFormatter()
        f.dateFormat = "M月d日"
        return f.string(from: Date(timeIntervalSince1970: ms / 1000))
    }

    private static func timeString(_ ms: Double) -> String {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f.string(from: Date(timeIntervalSince1970: ms / 1000))
    }
}

// MARK: - 小部件

final class ChipView: NSView {
    private let background = ColorView(fill: Theme.chip, radius: 8)
    private let label = NSTextField(labelWithString: "")
    private let icon = NSImageView()

    init(text: String, symbolName: String? = nil) {
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        addSubview(background)

        icon.image = symbolName.flatMap { symbol($0, size: 10) }
        icon.contentTintColor = .secondaryLabelColor
        icon.isHidden = symbolName == nil
        icon.translatesAutoresizingMaskIntoConstraints = false
        addSubview(icon)

        label.stringValue = text
        label.font = .systemFont(ofSize: 10.5)
        label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)

        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 24),
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.topAnchor.constraint(equalTo: topAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),

            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 12),
            icon.heightAnchor.constraint(equalToConstant: 12),

            label.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: symbolName == nil ? 0 : 5),
            label.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -9),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        if symbolName == nil {
            icon.widthAnchor.constraint(equalToConstant: 0).isActive = true
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    func setText(_ text: String) {
        label.stringValue = text
    }
}

// MARK: - 应用

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTextViewDelegate, NSSearchFieldDelegate {
    private var window: NSWindow!
    private let service = ChatService()

    private var root: ColorView!
    private var rail: ColorView!
    private var sidebar: ColorView!
    private var main: ColorView!

    private var contentScroll: NSScrollView!
    private var document: FlippedView!
    private var contentColumn: NSView!
    private var stack: NSStackView!
    private var emptyState: NSStackView!

    private var sessionsScroll: NSScrollView!
    private var sessionsStack: NSStackView!
    private var searchField: NSSearchField!
    private var searchVisible = false
    private var newChatTopToHeader: NSLayoutConstraint!
    private var newChatTopToSearch: NSLayoutConstraint!
    private var searchHeight: NSLayoutConstraint!

    private var composer: ColorView!
    private var composerTextView: NSTextView!
    private var composerPlaceholder = NSTextField(labelWithString: "随心输入")
    private var composerHeight: NSLayoutConstraint!
    private var textHeight: NSLayoutConstraint!
    private var sendButton: NSButton!
    private var effortPopup: NSPopUpButton!

    private var approvalCard: ColorView!
    private var approvalTitle = NSTextField(labelWithString: "需要你确认")
    private var approvalDetail = NSTextField(labelWithString: "")
    private var approvalPreview = NSTextField(wrappingLabelWithString: "")

    private var railStatusDot = NSView()

    private var rows: [MessageRowView] = []
    private var renderedIDs: [UUID] = []
    private var lastRenderedDate: Date?
    private var typingRow: TypingRowView?
    private var cancellables = Set<AnyCancellable>()
    private var capabilityWindow: CapabilityWindowController?

    func applicationDidFinishLaunching(_ notification: Notification) {
        Theme.applyEnvironmentAppearance()
        NSApp.applicationIconImage = Self.makeIcon()
        buildMenu()
        buildWindow()
        bind()
        service.start()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window.makeFirstResponder(composerTextView)
        if ProcessInfo.processInfo.environment["YUNXI_OPEN_CAPABILITIES"] == "1" {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { [weak self] in
                self?.showCapabilities()
            }
        }
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
        appMenu.addItem(withTitle: "能力面板", action: #selector(showCapabilities), keyEquivalent: "i")
        appMenu.addItem(.separator())
        let appearanceItem = NSMenuItem(title: "外观", action: nil, keyEquivalent: "")
        let appearanceMenu = NSMenu()
        for (title, key) in [("跟随系统", "system"), ("浅色", "light"), ("深色", "dark")] {
            let item = NSMenuItem(title: title, action: #selector(setAppearance(_:)), keyEquivalent: "")
            item.representedObject = key
            appearanceMenu.addItem(item)
        }
        appearanceItem.submenu = appearanceMenu
        appMenu.addItem(appearanceItem)
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "退出 云熙", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        NSApp.mainMenu = mainMenu
    }

    @objc private func setAppearance(_ sender: NSMenuItem) {
        guard let name = sender.representedObject as? String else { return }
        Theme.apply(name: name)
        UserDefaults.standard.set(name, forKey: "YunXiAppearance")
        window.backgroundColor = Theme.window
    }

    // MARK: 窗口

    private func buildWindow() {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1180, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.title = "云熙"
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.backgroundColor = Theme.window
        window.minSize = NSSize(width: 1000, height: 640)
        window.center()
        window.delegate = self
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("YunXiCodexShellV2")
        if window.frame.width < 1000 || window.frame.height < 640 {
            window.setContentSize(NSSize(width: 1180, height: 800))
            window.center()
        }
        window.appearance = NSApp.appearance

        root = ColorView(fill: Theme.window)
        window.contentView = root
        root.translatesAutoresizingMaskIntoConstraints = true
        root.autoresizingMask = [.width, .height]

        rail = ColorView(fill: Theme.rail)
        sidebar = ColorView(fill: Theme.sidebar)
        main = ColorView(fill: Theme.window)
        root.addSubview(rail)
        root.addSubview(sidebar)
        root.addSubview(main)

        let divider = ColorView(fill: Theme.border)
        root.addSubview(divider)

        NSLayoutConstraint.activate([
            rail.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            rail.topAnchor.constraint(equalTo: root.topAnchor),
            rail.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            rail.widthAnchor.constraint(equalToConstant: 56),

            sidebar.leadingAnchor.constraint(equalTo: rail.trailingAnchor),
            sidebar.topAnchor.constraint(equalTo: root.topAnchor),
            sidebar.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            sidebar.widthAnchor.constraint(equalToConstant: 280),

            divider.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            divider.topAnchor.constraint(equalTo: root.topAnchor),
            divider.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            divider.widthAnchor.constraint(equalToConstant: 1),

            main.leadingAnchor.constraint(equalTo: divider.trailingAnchor),
            main.topAnchor.constraint(equalTo: root.topAnchor),
            main.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            main.bottomAnchor.constraint(equalTo: root.bottomAnchor),
        ])

        buildRail()
        buildSidebar()
        buildMain()
    }

    private func buildRail() {
        let avatar = AvatarView(diameter: 28, fontSize: 11)
        rail.addSubview(avatar)
        NSLayoutConstraint.activate([
            avatar.centerXAnchor.constraint(equalTo: rail.centerXAnchor),
            avatar.topAnchor.constraint(equalTo: rail.topAnchor, constant: 52),
        ])

        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 8
        stack.translatesAutoresizingMaskIntoConstraints = false
        rail.addSubview(stack)

        let buttons: [(String, Selector, String, Bool)] = [
            ("bubble.left.and.bubble.right.fill", #selector(focusComposer), "对话", true),
            ("square.grid.2x2", #selector(showCapabilities), "能力", false),
            ("gearshape", #selector(showAbout), "关于", false),
        ]
        for item in buttons {
            let button = NSButton()
            button.image = symbol(item.0, size: 16)
            button.imageScaling = .scaleProportionallyUpOrDown
            button.isBordered = false
            button.bezelStyle = .regularSquare
            button.contentTintColor = item.3 ? .labelColor : .secondaryLabelColor
            button.target = self
            button.action = item.1
            button.toolTip = item.2
            button.translatesAutoresizingMaskIntoConstraints = false
            button.wantsLayer = true
            button.layer?.cornerRadius = 9
            if item.3 { button.layer?.backgroundColor = Theme.selection.cgColor }
            NSLayoutConstraint.activate([
                button.widthAnchor.constraint(equalToConstant: 36),
                button.heightAnchor.constraint(equalToConstant: 36),
            ])
            stack.addArrangedSubview(button)
        }

        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: avatar.bottomAnchor, constant: 20),
            stack.centerXAnchor.constraint(equalTo: rail.centerXAnchor),
        ])

        railStatusDot.wantsLayer = true
        railStatusDot.layer?.cornerRadius = 3
        railStatusDot.layer?.backgroundColor = NSColor.systemOrange.cgColor
        rail.addSubview(railStatusDot)
        NSLayoutConstraint.activate([
            railStatusDot.centerXAnchor.constraint(equalTo: rail.centerXAnchor),
            railStatusDot.bottomAnchor.constraint(equalTo: rail.bottomAnchor, constant: -18),
            railStatusDot.widthAnchor.constraint(equalToConstant: 6),
            railStatusDot.heightAnchor.constraint(equalToConstant: 6),
        ])
    }

    private func buildSidebar() {
        effortPopup = NSPopUpButton(frame: .zero, pullsDown: false)
        effortPopup.addItems(withTitles: ["自动", "思考开", "思考关"])
        effortPopup.isBordered = false
        effortPopup.controlSize = .small
        effortPopup.font = .systemFont(ofSize: 12)
        effortPopup.target = self
        effortPopup.action = #selector(effortChanged)
        effortPopup.translatesAutoresizingMaskIntoConstraints = false
        effortPopup.wantsLayer = true
        effortPopup.layer?.cornerRadius = 7
        effortPopup.layer?.backgroundColor = Theme.chip.cgColor
        sidebar.addSubview(effortPopup)

        let compose = NSButton()
        compose.image = symbol("square.and.pencil", size: 14)
        compose.contentTintColor = .secondaryLabelColor
        compose.isBordered = false
        compose.target = self
        compose.action = #selector(newChat)
        compose.toolTip = "新会话"
        compose.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(compose)

        let headerRow = NSStackView(views: [effortPopup, NSView(), compose])
        headerRow.orientation = .horizontal
        headerRow.alignment = .centerY
        headerRow.spacing = 6
        headerRow.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(headerRow)

        searchField = NSSearchField()
        searchField.placeholderString = "搜索"
        searchField.font = .systemFont(ofSize: 12)
        searchField.controlSize = .small
        searchField.translatesAutoresizingMaskIntoConstraints = false
        searchField.delegate = self
        searchField.focusRingType = .none
        searchField.isBordered = false
        searchField.drawsBackground = true
        searchField.backgroundColor = Theme.chip
        searchField.wantsLayer = true
        searchField.layer?.cornerRadius = 7
        sidebar.addSubview(searchField)

        let recent = NSTextField(labelWithString: "最近")
        recent.font = .systemFont(ofSize: 10.5)
        recent.textColor = .tertiaryLabelColor
        recent.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(recent)

        sessionsScroll = NSScrollView()
        sessionsScroll.hasVerticalScroller = true
        sessionsScroll.autohidesScrollers = true
        sessionsScroll.drawsBackground = false
        sessionsScroll.borderType = .noBorder
        sessionsScroll.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(sessionsScroll)

        let sessionsDoc = FlippedView()
        sessionsDoc.translatesAutoresizingMaskIntoConstraints = false
        sessionsScroll.documentView = sessionsDoc
        sessionsStack = NSStackView()
        sessionsStack.orientation = .vertical
        sessionsStack.alignment = .leading
        sessionsStack.spacing = 2
        sessionsStack.edgeInsets = NSEdgeInsetsZero
        sessionsStack.translatesAutoresizingMaskIntoConstraints = false
        sessionsDoc.addSubview(sessionsStack)
        NSLayoutConstraint.activate([
            sessionsDoc.widthAnchor.constraint(equalTo: sessionsScroll.contentView.widthAnchor),
            sessionsStack.topAnchor.constraint(equalTo: sessionsDoc.topAnchor, constant: 2),
            sessionsStack.leadingAnchor.constraint(equalTo: sessionsDoc.leadingAnchor),
            sessionsStack.trailingAnchor.constraint(equalTo: sessionsDoc.trailingAnchor),
            sessionsStack.bottomAnchor.constraint(equalTo: sessionsDoc.bottomAnchor),
        ])

        NSLayoutConstraint.activate([
            headerRow.topAnchor.constraint(equalTo: sidebar.topAnchor, constant: 52),
            headerRow.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 14),
            headerRow.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -14),

            effortPopup.widthAnchor.constraint(equalToConstant: 76),
            effortPopup.heightAnchor.constraint(equalToConstant: 26),
            compose.widthAnchor.constraint(equalToConstant: 24),
            compose.heightAnchor.constraint(equalToConstant: 24),

            searchField.topAnchor.constraint(equalTo: headerRow.bottomAnchor, constant: 10),
            searchField.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 12),
            searchField.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -12),
            searchField.heightAnchor.constraint(equalToConstant: 26),

            recent.topAnchor.constraint(equalTo: searchField.bottomAnchor, constant: 14),
            recent.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 16),

            sessionsScroll.topAnchor.constraint(equalTo: recent.bottomAnchor, constant: 6),
            sessionsScroll.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 8),
            sessionsScroll.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -8),
            sessionsScroll.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor, constant: -10),
        ])
    }

    private func buildMain() {
        contentScroll = NSScrollView()
        contentScroll.hasVerticalScroller = true
        contentScroll.autohidesScrollers = true
        contentScroll.drawsBackground = false
        contentScroll.borderType = .noBorder
        contentScroll.translatesAutoresizingMaskIntoConstraints = false
        main.addSubview(contentScroll)

        document = FlippedView()
        document.translatesAutoresizingMaskIntoConstraints = false
        contentScroll.documentView = document

        contentColumn = NSView()
        contentColumn.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(contentColumn)

        stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 12
        stack.edgeInsets = NSEdgeInsetsZero
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentColumn.addSubview(stack)

        let preferredWidth = contentColumn.widthAnchor.constraint(equalTo: document.widthAnchor, constant: -56)
        preferredWidth.priority = .defaultHigh
        NSLayoutConstraint.activate([
            document.widthAnchor.constraint(equalTo: contentScroll.contentView.widthAnchor),

            contentColumn.topAnchor.constraint(equalTo: document.topAnchor, constant: 24),
            contentColumn.bottomAnchor.constraint(equalTo: document.bottomAnchor, constant: -12),
            contentColumn.centerXAnchor.constraint(equalTo: document.centerXAnchor),
            contentColumn.widthAnchor.constraint(lessThanOrEqualToConstant: 820),
            contentColumn.leadingAnchor.constraint(greaterThanOrEqualTo: document.leadingAnchor, constant: 24),
            contentColumn.trailingAnchor.constraint(lessThanOrEqualTo: document.trailingAnchor, constant: -24),
            preferredWidth,

            stack.topAnchor.constraint(equalTo: contentColumn.topAnchor),
            stack.leadingAnchor.constraint(equalTo: contentColumn.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: contentColumn.trailingAnchor),
            stack.bottomAnchor.constraint(equalTo: contentColumn.bottomAnchor),
        ])

        buildEmptyState()
        buildComposer()
        buildApprovalCard()
    }

    private func buildEmptyState() {
        let cloud = NSImageView()
        cloud.image = symbol("cloud", size: 42, weight: .thin)
        cloud.contentTintColor = .tertiaryLabelColor
        cloud.translatesAutoresizingMaskIntoConstraints = false

        let heading = NSTextField(labelWithString: "今天想聊点什么？")
        heading.font = .systemFont(ofSize: 24, weight: .semibold)
        let sub = NSTextField(labelWithString: "说事、闲聊、安排今天，都可以。")
        sub.font = .systemFont(ofSize: 12.5)
        sub.textColor = .secondaryLabelColor

        emptyState = NSStackView(views: [cloud, heading, sub])
        emptyState.orientation = .vertical
        emptyState.alignment = .centerX
        emptyState.spacing = 10
        emptyState.translatesAutoresizingMaskIntoConstraints = false
        main.addSubview(emptyState)
        NSLayoutConstraint.activate([
            cloud.widthAnchor.constraint(equalToConstant: 48),
            cloud.heightAnchor.constraint(equalToConstant: 48),
            emptyState.centerXAnchor.constraint(equalTo: main.centerXAnchor),
            emptyState.centerYAnchor.constraint(equalTo: main.centerYAnchor, constant: -30),
        ])
    }

    private func buildComposer() {
        composer = ColorView(fill: Theme.elevated, radius: 16, border: Theme.border, borderWidth: 0.8)
        composer.layer?.shadowColor = NSColor.black.cgColor
        composer.layer?.shadowOpacity = 0.16
        composer.layer?.shadowRadius = 20
        composer.layer?.shadowOffset = CGSize(width: 0, height: 6)
        main.addSubview(composer)

        let textScroll = NSScrollView()
        textScroll.hasVerticalScroller = false
        textScroll.drawsBackground = false
        textScroll.borderType = .noBorder
        textScroll.translatesAutoresizingMaskIntoConstraints = false
        composer.addSubview(textScroll)

        composerTextView = NSTextView()
        composerTextView.isRichText = false
        composerTextView.isEditable = true
        composerTextView.isSelectable = true
        composerTextView.drawsBackground = false
        composerTextView.font = .systemFont(ofSize: 14)
        composerTextView.textColor = .labelColor
        composerTextView.insertionPointColor = Theme.accent
        composerTextView.textContainerInset = NSSize(width: 0, height: 3)
        composerTextView.isVerticallyResizable = true
        composerTextView.isHorizontallyResizable = false
        composerTextView.autoresizingMask = [.width]
        composerTextView.textContainer?.widthTracksTextView = true
        composerTextView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        composerTextView.delegate = self
        textScroll.documentView = composerTextView

        composerPlaceholder = NSTextField(labelWithString: "说点什么…")
        composerPlaceholder.font = .systemFont(ofSize: 14)
        composerPlaceholder.textColor = .tertiaryLabelColor
        composerPlaceholder.translatesAutoresizingMaskIntoConstraints = false
        composer.addSubview(composerPlaceholder)

        sendButton = NSButton()
        sendButton.image = symbol("arrow.up", size: 12, weight: .semibold)
        sendButton.contentTintColor = .white
        sendButton.isBordered = false
        sendButton.wantsLayer = true
        sendButton.layer?.cornerRadius = 14
        sendButton.layer?.backgroundColor = Theme.accent.cgColor
        sendButton.target = self
        sendButton.action = #selector(sendMessage)
        sendButton.toolTip = "发送（Enter）"
        sendButton.translatesAutoresizingMaskIntoConstraints = false
        composer.addSubview(sendButton)

        let hint = NSTextField(labelWithString: "仅本机 · Enter 发送、Shift+Enter 换行")
        hint.font = .systemFont(ofSize: 9.5)
        hint.textColor = .tertiaryLabelColor
        hint.translatesAutoresizingMaskIntoConstraints = false
        main.addSubview(hint)

        composerHeight = composer.heightAnchor.constraint(equalToConstant: 56)
        composerHeight.isActive = true
        textHeight = textScroll.heightAnchor.constraint(equalToConstant: 28)
        textHeight.isActive = true

        NSLayoutConstraint.activate([
            contentScroll.topAnchor.constraint(equalTo: main.topAnchor),
            contentScroll.leadingAnchor.constraint(equalTo: main.leadingAnchor),
            contentScroll.trailingAnchor.constraint(equalTo: main.trailingAnchor),
            contentScroll.bottomAnchor.constraint(equalTo: composer.topAnchor, constant: -12),

            composer.leadingAnchor.constraint(greaterThanOrEqualTo: main.leadingAnchor, constant: 32),
            composer.trailingAnchor.constraint(lessThanOrEqualTo: main.trailingAnchor, constant: -32),
            composer.centerXAnchor.constraint(equalTo: main.centerXAnchor),
            composer.widthAnchor.constraint(lessThanOrEqualToConstant: 720),
            composer.widthAnchor.constraint(greaterThanOrEqualToConstant: 520),
            composer.bottomAnchor.constraint(equalTo: hint.topAnchor, constant: -8),

            hint.centerXAnchor.constraint(equalTo: main.centerXAnchor),
            hint.bottomAnchor.constraint(equalTo: main.bottomAnchor, constant: -10),

            textScroll.leadingAnchor.constraint(equalTo: composer.leadingAnchor, constant: 16),
            textScroll.trailingAnchor.constraint(equalTo: sendButton.leadingAnchor, constant: -8),
            textScroll.topAnchor.constraint(equalTo: composer.topAnchor, constant: 12),
            textScroll.bottomAnchor.constraint(equalTo: composer.bottomAnchor, constant: -12),

            composerPlaceholder.leadingAnchor.constraint(equalTo: composer.leadingAnchor, constant: 17),
            composerPlaceholder.topAnchor.constraint(equalTo: textScroll.topAnchor, constant: 5),

            sendButton.trailingAnchor.constraint(equalTo: composer.trailingAnchor, constant: -12),
            sendButton.centerYAnchor.constraint(equalTo: composer.centerYAnchor),
            sendButton.widthAnchor.constraint(equalToConstant: 28),
            sendButton.heightAnchor.constraint(equalToConstant: 28),
        ])
    }

    private func buildApprovalCard() {
        approvalCard = ColorView(fill: Theme.elevated, radius: 16, border: Theme.border, borderWidth: 1)
        approvalCard.layer?.shadowColor = NSColor.black.cgColor
        approvalCard.layer?.shadowOpacity = 0.14
        approvalCard.layer?.shadowRadius = 16
        approvalCard.layer?.shadowOffset = CGSize(width: 0, height: 5)
        approvalCard.isHidden = true
        main.addSubview(approvalCard)

        approvalTitle.font = .systemFont(ofSize: 12.5, weight: .semibold)
        approvalDetail.font = .systemFont(ofSize: 10.5)
        approvalDetail.textColor = .secondaryLabelColor
        approvalDetail.lineBreakMode = .byTruncatingTail
        approvalPreview.font = .monospacedSystemFont(ofSize: 10.5, weight: .regular)
        approvalPreview.textColor = .secondaryLabelColor
        approvalPreview.maximumNumberOfLines = 5
        approvalPreview.lineBreakMode = .byTruncatingTail
        approvalPreview.preferredMaxLayoutWidth = 640

        let allow = NSButton(title: "允许", target: self, action: #selector(approveAction))
        allow.bezelStyle = .rounded
        allow.controlSize = .small
        let deny = NSButton(title: "拒绝", target: self, action: #selector(denyAction))
        deny.bezelStyle = .rounded
        deny.controlSize = .small
        let buttons = NSStackView(views: [deny, allow])
        buttons.orientation = .horizontal
        buttons.spacing = 6
        buttons.alignment = .trailing
        buttons.translatesAutoresizingMaskIntoConstraints = false

        let content = NSStackView(views: [approvalTitle, approvalDetail, approvalPreview, buttons])
        content.orientation = .vertical
        content.alignment = .leading
        content.spacing = 5
        content.translatesAutoresizingMaskIntoConstraints = false
        approvalCard.addSubview(content)

        NSLayoutConstraint.activate([
            approvalCard.leadingAnchor.constraint(equalTo: composer.leadingAnchor),
            approvalCard.trailingAnchor.constraint(equalTo: composer.trailingAnchor),
            approvalCard.bottomAnchor.constraint(equalTo: composer.topAnchor, constant: -8),

            content.leadingAnchor.constraint(equalTo: approvalCard.leadingAnchor, constant: 12),
            content.trailingAnchor.constraint(equalTo: approvalCard.trailingAnchor, constant: -12),
            content.topAnchor.constraint(equalTo: approvalCard.topAnchor, constant: 10),
            content.bottomAnchor.constraint(equalTo: approvalCard.bottomAnchor, constant: -10),
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
            .sink { [weak self] value in
                self?.railStatusDot.layer?.backgroundColor = (value == "在线" ? NSColor.systemGreen : NSColor.systemOrange).cgColor
            }
            .store(in: &cancellables)
        service.$sessions
            .receive(on: RunLoop.main)
            .sink { [weak self] sessions in self?.reloadSessions(sessions) }
            .store(in: &cancellables)
        service.$session
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.reloadSessions(self?.service.sessions ?? []) }
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

        emptyState.isHidden = messages.contains { $0.role != .system }
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
        row.alphaValue = 0
        stack.addArrangedSubview(row)
        row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        rows.append(row)
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0.18
            row.animator().alphaValue = 1
        }
    }

    private func updateTyping(visible: Bool) {
        if visible, typingRow == nil {
            let row = TypingRowView()
            row.alphaValue = 0
            stack.addArrangedSubview(row)
            row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
            typingRow = row
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.16
                row.animator().alphaValue = 1
            }
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
            let visible = self.contentScroll.contentView.bounds.height
            let maxY = max(0, height - visible + 40)
            self.contentScroll.contentView.scroll(to: NSPoint(x: 0, y: maxY))
            self.contentScroll.reflectScrolledClipView(self.contentScroll.contentView)
        }
    }

    // MARK: 会话侧栏

    private func reloadSessions(_ sessions: [ChatSession]) {
        for view in sessionsStack.arrangedSubviews {
            sessionsStack.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
        let query = searchField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        let filtered = query.isEmpty
            ? sessions
            : sessions.filter { $0.preview.localizedCaseInsensitiveContains(query) || $0.id.localizedCaseInsensitiveContains(query) }

        if filtered.isEmpty {
            let hint = NSTextField(wrappingLabelWithString: query.isEmpty ? "还没有会话。\n点右上角新建开始。" : "没有匹配的会话。")
            hint.font = .systemFont(ofSize: 11)
            hint.textColor = .tertiaryLabelColor
            hint.alignment = .center
            sessionsStack.addArrangedSubview(hint)
            hint.widthAnchor.constraint(equalTo: sessionsStack.widthAnchor).isActive = true
            return
        }

        var lastGroup = ""
        let calendar = Calendar.current
        for session in filtered {
            let group = groupTitle(for: session.updatedAt, calendar: calendar)
            if group != lastGroup {
                let label = NSTextField(labelWithString: group)
                label.font = .systemFont(ofSize: 10.5, weight: .medium)
                label.textColor = .tertiaryLabelColor
                sessionsStack.addArrangedSubview(label)
                label.widthAnchor.constraint(equalTo: sessionsStack.widthAnchor).isActive = true
                lastGroup = group
            }
            let row = SessionRowView(session: session, selected: session.id == service.session) { [weak self] in
                guard let self, !self.service.busy else { return }
                self.service.openSession(session.id)
            }
            sessionsStack.addArrangedSubview(row)
            row.widthAnchor.constraint(equalTo: sessionsStack.widthAnchor).isActive = true
        }
    }

    private func groupTitle(for ms: Double, calendar: Calendar) -> String {
        let date = Date(timeIntervalSince1970: ms / 1000)
        if calendar.isDateInToday(date) { return "今天" }
        if calendar.isDateInYesterday(date) { return "昨天" }
        return "更早"
    }

    @objc private func toggleSearch() {
        searchVisible.toggle()
        searchField.isHidden = !searchVisible
        searchHeight.constant = searchVisible ? 26 : 0
        newChatTopToHeader.isActive = !searchVisible
        newChatTopToSearch.isActive = searchVisible
        if searchVisible {
            window.makeFirstResponder(searchField)
            searchField.stringValue = ""
        } else {
            reloadSessions(service.sessions)
            window.makeFirstResponder(composerTextView)
        }
    }

    func controlTextDidChange(_ obj: Notification) {
        if obj.object as? NSSearchField === searchField {
            reloadSessions(service.sessions)
        }
    }

    // MARK: 审批

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

    // MARK: 输入

    func textDidChange(_ notification: Notification) {
        let text = composerTextView.string
        composerPlaceholder.isHidden = !text.isEmpty
        let used = composerTextView.layoutManager?.usedRect(for: composerTextView.textContainer!) ?? .zero
        let height = min(120, max(28, used.height + 8))
        let composerTarget = height + 24
        if abs(textHeight.constant - height) > 0.5 {
            textHeight.constant = height
            composerHeight.constant = composerTarget
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
        resetComposer()
        service.send(text)
    }

    private func resetComposer() {
        composerTextView.string = ""
        composerPlaceholder.isHidden = false
        textHeight.constant = 28
        composerHeight.constant = 56
        sendButton.isEnabled = false
        window.makeFirstResponder(composerTextView)
    }

    @objc private func useSuggestion(_ sender: NSButton) {
        guard service.connected, !service.busy else { return }
        service.send(sender.title)
    }

    @objc private func effortChanged() {
        let values = ["auto", "on", "off"]
        let index = effortPopup.indexOfSelectedItem
        if index >= 0 && index < values.count {
            service.setEffort(values[index])
        }
    }

    @objc private func newChat() {
        service.newChat()
        window.makeFirstResponder(composerTextView)
    }

    @objc private func focusComposer() {
        window.makeFirstResponder(composerTextView)
    }

    @objc private func focusSessions() {
        sessionsScroll.contentView.scroll(to: .zero)
        sessionsScroll.reflectScrolledClipView(sessionsScroll.contentView)
    }

    @objc private func showAbout() {
        NSApp.orderFrontStandardAboutPanel(nil)
    }

    @objc private func showCapabilities() {
        if capabilityWindow == nil {
            capabilityWindow = CapabilityWindowController(service: service)
        }
        capabilityWindow?.showWindow(nil)
        capabilityWindow?.window?.makeKeyAndOrderFront(nil)
        service.requestCapabilities()
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
