import Foundation
import UserNotifications

@MainActor enum NativeNotifications {
    static func status() async -> [String: Any] {
        let settings = await UNUserNotificationCenter.current().notificationSettings()
        let authorization: String
        switch settings.authorizationStatus {
        case .authorized: authorization = "full_access"
        case .provisional: authorization = "provisional"
        case .denied: authorization = "denied"
        case .notDetermined: authorization = "not_determined"
        default: authorization = "unavailable"
        }
        return ["notifications": authorization, "alert_enabled": settings.alertSetting == .enabled,
                "notification_center_enabled": settings.notificationCenterSetting == .enabled,
                "sound_enabled": settings.soundSetting == .enabled, "alert_style": settings.alertStyle.rawValue,
                "backend": "UNUserNotificationCenter", "bundle_id": Bundle.main.bundleIdentifier ?? "unavailable"]
    }

    static func authorize() async throws -> [String: Any] {
        let center = UNUserNotificationCenter.current()
        if await center.notificationSettings().authorizationStatus == .notDetermined {
            _ = try await center.requestAuthorization(options: [.alert, .sound, .badge])
        }
        return await status()
    }

    static func send(_ args: [String: Any]) async throws -> [String: Any] {
        guard Set(args.keys).isSubset(of: ["title", "body", "tag"]) else { throw Failure("bad_args", "未知通知参数") }
        let title = try text(args, "title", max: 500), body = try text(args, "body", max: 4000)
        guard let tag = args["tag"] as? String, tag.utf8.count <= 4000, !tag.contains("\0") else { throw Failure("bad_args", "通知标识无效") }
        let settings = await status()
        guard ["full_access", "provisional"].contains(settings["notifications"] as? String ?? "") else {
            return ["kind": "blocked", "reason": "云熙通知权限未开启，请运行 mac authorize notifications，并在系统设置中允许通知。", "settings": settings]
        }
        let center = UNUserNotificationCenter.current()
        let identifier = tag.isEmpty ? UUID().uuidString : try digest(["tag": tag])
        func matches(_ notice: UNNotification) -> Bool {
            notice.request.identifier == identifier && notice.request.content.title == title && notice.request.content.body == body
        }
        // 相同业务标识且内容相同的回读用于重试去重，不再弹第二条。
        if await center.deliveredNotifications().contains(where: matches) {
            return ["kind": "confirmed", "evidence": "通知中心已存在同标识与相同内容；未重复投递。不证明横幅可见或用户已读。", "identifier": identifier, "replayed": true, "settings": settings]
        }
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        try await center.add(UNNotificationRequest(identifier: identifier, content: content, trigger: nil))
        for _ in 0..<10 {
            if await center.deliveredNotifications().contains(where: matches) {
                return ["kind": "confirmed", "evidence": "已在云熙的macOS通知中心记录中回读相同标识和内容；不证明横幅可见或用户已读。", "identifier": identifier, "replayed": false, "settings": settings]
            }
            try await Task.sleep(nanoseconds: 200_000_000)
        }
        return ["kind": "handed_off", "note": "系统接受通知，但暂未在通知中心回读到；无法确认横幅或用户已读。", "identifier": identifier, "settings": settings]
    }
}
