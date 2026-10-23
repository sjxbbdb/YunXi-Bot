import AppKit

let app = NSApplication.shared
// 仅测试/截图时允许指定外观；正常使用完全跟随系统。
if let appearance = ProcessInfo.processInfo.environment["YUNXI_APPEARANCE"] {
    switch appearance {
    case "dark": app.appearance = NSAppearance(named: .darkAqua)
    case "light": app.appearance = NSAppearance(named: .aqua)
    default: break
    }
}
let delegate = MacAppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
