import AppKit

let app = NSApplication.shared

// 外观必须在建窗之前确定；否则 layer 颜色和文字颜色会按不同外观解析。
let envAppearance = ProcessInfo.processInfo.environment["YUNXI_APPEARANCE"]
let savedAppearance = UserDefaults.standard.string(forKey: "YunXiAppearance")
switch envAppearance ?? savedAppearance ?? "dark" {
case "dark": app.appearance = NSAppearance(named: .darkAqua)
case "light": app.appearance = NSAppearance(named: .aqua)
default: app.appearance = nil
}

let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
