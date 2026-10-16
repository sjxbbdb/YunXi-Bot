import AppKit
import CryptoKit
import Contacts
import EventKit
import Foundation

struct Failure: Error {
    let code: String
    let message: String
    init(_ code: String, _ message: String) { self.code = code; self.message = message }
}

func encoded(_ value: Any) throws -> Data {
    try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
}

func digest(_ value: Any) throws -> String {
    SHA256.hash(data: try encoded(value)).map { String(format: "%02x", $0) }.joined()
}

func text(_ args: [String: Any], _ key: String, max: Int = 2000) throws -> String {
    guard let s = args[key] as? String, !s.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
          s.utf8.count <= max, !s.contains("\0") else { throw Failure("bad_args", "\(key) 必须是非空文本，最多 \(max) 字节") }
    return s
}

func date(_ s: String) throws -> Date {
    let parser = ISO8601DateFormatter()
    guard let d = parser.date(from: s), s.contains("T"),
          s.hasSuffix("Z") || s.range(of: "[+-][0-9]{2}:[0-9]{2}$", options: .regularExpression) != nil else {
        throw Failure("bad_args", "时间必须带时区，例如 2026-10-16T15:00:00+08:00")
    }
    return d
}

func stamp(_ d: Date) -> String { ISO8601DateFormatter().string(from: d) }

func reminderMatches(notes: String?, completed: Bool, args: [String: Any]) -> Bool {
    (args["include_completed"] as? Bool == true || !completed) &&
    ((args["notes_prefix"] as? String).map { (notes ?? "").hasPrefix($0) } ?? true)
}

func validate(_ action: String, _ args: [String: Any]) throws {
    let fields: [String: Set<String>] = [
        "mac_status": [], "authorize": ["target"],
        "mac_calendar_list": ["start", "end"],
        "mac_calendar_create": ["title", "start", "end", "notes", "request_id"],
        "mac_calendar_update": ["id", "expected_revision", "title", "start", "end", "notes"],
        "mac_calendar_delete": ["id", "expected_revision"],
        "mac_reminders_list": ["include_completed", "notes_prefix"],
        "mac_reminder_create": ["title", "due_at", "notes", "request_id"],
        "mac_reminder_update": ["id", "expected_revision", "title", "due_at", "notes", "completed"],
        "mac_reminder_delete": ["id", "expected_revision"]
    ]
    guard let allowed = fields[action], Set(args.keys).isSubset(of: allowed) else {
        throw Failure("bad_args", "未知动作或多余参数")
    }
    for key in ["title", "id", "expected_revision", "request_id", "target", "notes_prefix"] where args[key] != nil {
        _ = try text(args, key, max: key == "title" ? 500 : 200)
    }
    if let notes = args["notes"] { guard let s = notes as? String, s.utf8.count <= 4000, !s.contains("\0") else { throw Failure("bad_args", "notes 最多 4000 字节") } }
    for key in ["start", "end", "due_at"] where args[key] != nil {
        if key == "due_at", args[key] is NSNull { continue }
        _ = try date(text(args, key))
    }
    for key in ["completed", "include_completed"] where args[key] != nil {
        guard let value = args[key] as? NSNumber, CFGetTypeID(value) == CFBooleanGetTypeID() else { throw Failure("bad_args", "\(key) 必须是布尔值") }
    }
    if action.hasSuffix("_create") {
        _ = try text(args, "title", max: 500)
        let key = try text(args, "request_id", max: 100)
        guard key.range(of: "^[A-Za-z0-9_-]{8,100}$", options: .regularExpression) != nil else { throw Failure("bad_args", "request_id 需为 8–100 位字母、数字、下划线或连字符；重试沿用原值") }
    }
    if action.hasSuffix("_update") || action.hasSuffix("_delete") {
        _ = try text(args, "id"); _ = try text(args, "expected_revision")
        if action.hasSuffix("_update"), args.keys.allSatisfy({ ["id", "expected_revision"].contains($0) }) { throw Failure("bad_args", "没有要修改的字段") }
    }
    if action == "mac_calendar_list" || action == "mac_calendar_create" {
        let start = try date(text(args, "start")), end = try date(text(args, "end"))
        guard end > start, end.timeIntervalSince(start) <= 31 * 86400 else { throw Failure("bad_args", "结束必须晚于开始，跨度最多 31 天") }
    }
    if action == "authorize", !["calendar", "reminders", "contacts", "mail", "notifications"].contains(try text(args, "target")) { throw Failure("bad_args", "target 只接受 calendar/reminders/contacts/mail/notifications") }
}

@MainActor
final class Bridge {
    let store = EKEventStore()
    let fm = FileManager.default
    let listName = "云熙"

    func authorization(_ kind: EKEntityType) -> String {
        switch EKEventStore.authorizationStatus(for: kind) {
        case .fullAccess: return "full_access"
        case .writeOnly: return "write_only"
        case .denied: return "denied"
        case .restricted: return "restricted"
        case .notDetermined: return "not_determined"
        @unknown default: return "unavailable"
        }
    }

    func require(_ kind: EKEntityType) throws {
        guard EKEventStore.authorizationStatus(for: kind) == .fullAccess else {
            throw Failure("permission_required", "系统权限未就绪（\(authorization(kind))）；请运行 ./启动.command mac authorize \(kind == .event ? "calendar" : "reminders")。未执行读写。")
        }
    }

    func status() -> [String: Any] {
        var result: [String: Any] = ["calendar": authorization(.event), "reminders": authorization(.reminder), "write_scope": "icloud_yunxi_only"]
        for (name, kind) in [("calendar", EKEntityType.event), ("reminders", EKEntityType.reminder)] where authorization(kind) == "full_access" {
            result[name + "_icloud_sources"] = sources(kind).map { ["id": $0.sourceIdentifier, "title": $0.title] }
            result[name + "_managed_lists"] = store.calendars(for: kind).filter { $0.title == listName && $0.source.title == "iCloud" }.map { ["id": $0.calendarIdentifier, "title": $0.title, "writable": $0.allowsContentModifications] as [String: Any] }
        }
        return result
    }

    func sources(_ kind: EKEntityType) -> [EKSource] {
        // 两种实体的 iCloud source 可能不同；只从该实体的现有账户列表取来源。
        let candidates = store.calendars(for: kind).compactMap { $0.source }.filter { $0.title == "iCloud" && $0.sourceType == .calDAV }
        return Array(Dictionary(candidates.map { ($0.sourceIdentifier, $0) }, uniquingKeysWith: { first, _ in first }).values)
    }

    func managedCalendar(_ kind: EKEntityType) throws -> EKCalendar {
        let available = sources(kind)
        guard available.count == 1, let source = available.first else { throw Failure("account_unavailable", "没有唯一可用的 iCloud 账户；未回退到 Google 或其他账户") }
        let matches = store.calendars(for: kind).filter { $0.title == listName && $0.source.sourceIdentifier == source.sourceIdentifier }
        guard matches.count <= 1 else { throw Failure("ambiguous_list", "存在多个iCloud云熙列表，请先消除同名歧义") }
        if let calendar = matches.first {
            guard calendar.allowsContentModifications else { throw Failure("read_only", "iCloud云熙列表不可写") }
            return calendar
        }
        let calendar = EKCalendar(for: kind, eventStore: store)
        calendar.title = listName; calendar.source = source
        try store.saveCalendar(calendar, commit: true)
        guard calendar.source.sourceIdentifier == source.sourceIdentifier else { throw Failure("account_unavailable", "系统保存的账户不符合预期，停止执行") }
        return calendar
    }

    func owned(_ item: EKCalendarItem) -> Bool {
        item.calendar.source.title == "iCloud" && item.calendar.source.sourceType == .calDAV && item.calendar.title == listName && item.url?.scheme == "yunxi-bot" && item.url?.host == "icloud"
    }

    func record(_ item: EKCalendarItem) throws -> [String: Any] {
        var out: [String: Any] = ["id": item.calendarItemIdentifier, "title": item.title ?? "", "notes": item.notes ?? "", "calendar": item.calendar.title, "calendar_id": item.calendar.calendarIdentifier, "account": item.calendar.source.title, "local_only": item.calendar.source.sourceType == .local, "managed": owned(item), "modified_at": item.lastModifiedDate.map(stamp) as Any? ?? NSNull()]
        if let event = item as? EKEvent {
            out["start"] = stamp(event.startDate); out["end"] = stamp(event.endDate)
            out["all_day"] = event.isAllDay
            out["has_attendees"] = event.hasAttendees
        }
        if let reminder = item as? EKReminder {
            out["completed"] = reminder.isCompleted
            out["due_at"] = reminder.dueDateComponents.flatMap { $0.date }.map(stamp) as Any? ?? NSNull()
        }
        out["revision"] = try digest(["record": out, "modified": item.lastModifiedDate?.timeIntervalSince1970 as Any? ?? NSNull(), "alarms": (item.alarms ?? []).map { ["absolute": $0.absoluteDate?.timeIntervalSince1970 as Any? ?? NSNull(), "relative": $0.relativeOffset] }])
        // 返回正文有上限；版本摘要仍覆盖完整原文，不能漏掉被截断部分的修改。
        out["content_truncated"] = (item.notes ?? "").count > 4000 || (item.title ?? "").count > 500
        out["notes"] = String((item.notes ?? "").prefix(4000))
        out["title"] = String((item.title ?? "").prefix(500))
        return out
    }

    func reminders(_ calendars: [EKCalendar]? = nil) async throws -> [EKReminder] {
        let predicate = store.predicateForReminders(in: calendars)
        let result: [EKReminder]? = await withCheckedContinuation { continuation in
            store.fetchReminders(matching: predicate) { continuation.resume(returning: $0) }
        }
        guard let result else { throw Failure("read_failed", "提醒事项读取失败，不能当作空列表") }
        return result
    }

    func checkedItem(_ args: [String: Any], kind: EKEntityType) throws -> EKCalendarItem {
        guard let item = store.calendarItem(withIdentifier: try text(args, "id")), (kind == .event ? item is EKEvent : item is EKReminder) else { throw Failure("not_found", "记录不存在；请重新查询，不能声称修改或删除成功") }
        guard owned(item), item.calendar.allowsContentModifications else { throw Failure("not_managed", "只可修改iCloud云熙列表中由 Bot 创建的记录") }
        guard !item.hasRecurrenceRules else { throw Failure("unsupported", "首版不修改重复事项，避免误改整个系列") }
        if let event = item as? EKEvent, event.hasAttendees { throw Failure("unsupported", "首版不修改带参与者的日程") }
        guard try record(item)["revision"] as? String == text(args, "expected_revision") else { throw Failure("conflict", "记录已变化；请重新读取并重新安排，未覆盖已有修改") }
        return item
    }

    func setFields(_ item: EKCalendarItem, _ args: [String: Any]) throws {
        if let title = args["title"] as? String { item.title = title }
        if let notes = args["notes"] as? String { item.notes = notes }
        if let event = item as? EKEvent {
            if let start = args["start"] as? String { event.startDate = try date(start) }
            if let end = args["end"] as? String { event.endDate = try date(end) }
            guard let start = event.startDate, let end = event.endDate, end > start, end.timeIntervalSince(start) <= 31 * 86400 else { throw Failure("bad_args", "日程时间范围非法；未保存") }
        }
        if let reminder = item as? EKReminder {
            if let due = args["due_at"] {
                if due is NSNull { reminder.dueDateComponents = nil; reminder.alarms = nil }
                else {
                    let instant = try date(text(args, "due_at"))
                    var calendar = Calendar(identifier: .gregorian); calendar.timeZone = TimeZone.current
                    reminder.dueDateComponents = calendar.dateComponents([.calendar, .timeZone, .year, .month, .day, .hour, .minute, .second], from: instant)
                    reminder.alarms = [EKAlarm(absoluteDate: instant)]
                }
            }
            if let completed = args["completed"] as? Bool { reminder.isCompleted = completed }
        }
    }

    func save(_ item: EKCalendarItem) throws {
        if let event = item as? EKEvent { try store.save(event, span: .thisEvent, commit: true) }
        else if let reminder = item as? EKReminder { try store.save(reminder, commit: true) }
    }

    func run(_ action: String, _ args: [String: Any]) async throws -> [String: Any] {
        if action == "notification_send" { return try await NativeNotifications.send(args) }
        if let definition = try assistantDefinitions().first(where: { $0["name"] as? String == action }) {
            return try await AssistantBridge.run(action, args, definition: definition)
        }
        try validate(action, args)
        if action == "mac_status" {
            var result = status()
            result["notification"] = await NativeNotifications.status()
            result["contacts"] = AssistantBridge.contactsStatus()
            result["mail"] = await AssistantBridge.mailPermission(ask: false)
            result["mail_write_scope"] = "approved_message_changes; yunxi_local_drafts; approved_send_via_mail"
            return result
        }
        if action == "authorize" {
            if try text(args, "target") == "notifications" { return try await NativeNotifications.authorize() }
            if ["contacts", "mail"].contains(try text(args, "target")) { return try await AssistantBridge.authorize(text(args, "target")) }
            let kind: EKEntityType = try text(args, "target") == "calendar" ? .event : .reminder
            if EKEventStore.authorizationStatus(for: kind) == .notDetermined || EKEventStore.authorizationStatus(for: kind) == .writeOnly {
                if kind == .event { _ = try await store.requestFullAccessToEvents() }
                else { _ = try await store.requestFullAccessToReminders() }
            }
            return status()
        }
        let kind: EKEntityType = action.hasPrefix("mac_calendar_") ? .event : .reminder
        try require(kind)
        if action == "mac_calendar_list" {
            let matches = store.events(matching: store.predicateForEvents(withStart: try date(text(args, "start")), end: try date(text(args, "end")), calendars: nil)).sorted { $0.startDate < $1.startDate }
            return ["items": try matches.prefix(200).map(record), "truncated": matches.count > 200, "read_scope": "mac_event_store_including_synced_accounts"]
        }
        if action == "mac_reminders_list" {
            let matches = try await reminders().filter {
                reminderMatches(notes: $0.notes, completed: $0.isCompleted, args: args)
            }.sorted { $0.calendarItemIdentifier < $1.calendarItemIdentifier }
            return ["items": try matches.prefix(200).map(record), "truncated": matches.count > 200, "read_scope": "mac_event_store_including_synced_accounts"]
        }

        // 不同 CLI/daemon 也可能同时写：锁覆盖版本检查、系统保存和创建回执。
        let state = fm.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/YunXiBot/native", isDirectory: true)
        try fm.createDirectory(at: state, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let lock = Darwin.open(state.appendingPathComponent("write.lock").path, O_CREAT | O_RDWR, 0o600)
        guard lock >= 0 else { throw Failure("state_failed", "无法打开写入锁") }
        defer { Darwin.close(lock) }
        guard flock(lock, LOCK_EX | LOCK_NB) == 0 else { throw Failure("busy", "另一项本机写入正在进行，请稍后重新读取") }
        defer { flock(lock, LOCK_UN) }

        if action.hasSuffix("_create") {
            let requestID = try text(args, "request_id")
            let receipt = state.appendingPathComponent("\(action)-\(requestID).json")
            let fingerprint = try digest(args)
            if fm.fileExists(atPath: receipt.path) {
                guard let saved = try JSONSerialization.jsonObject(with: Data(contentsOf: receipt)) as? [String: Any], saved["fingerprint"] as? String == fingerprint else { throw Failure("request_conflict", "request_id 已用于不同参数；未创建新记录") }
                guard let id = saved["id"] as? String else { throw Failure("outcome_unknown", "上次创建未取得完整回执；请查询列表核对，勿换 request_id 盲目重试") }
                guard let item = store.calendarItem(withIdentifier: id), owned(item) else { throw Failure("not_found", "先前创建的记录已不存在或已移出管理范围；不会自动重建") }
                return ["item": try record(item), "created": false, "replayed": true]
            }
            let item: EKCalendarItem = kind == .event ? EKEvent(eventStore: store) : EKReminder(eventStore: store)
            item.calendar = try managedCalendar(kind)
            item.url = URL(string: "yunxi-bot://icloud/\(requestID)")
            try setFields(item, args)
            // 先留待定回执；进程若在系统提交后退出，重试也不能重复创建。
            try encoded(["fingerprint": fingerprint]).write(to: receipt, options: [.atomic])
            try save(item)
            guard let readBack = store.calendarItem(withIdentifier: item.calendarItemIdentifier), owned(readBack) else { throw Failure("outcome_unknown", "保存后未读回，结果待核对") }
            try encoded(["fingerprint": fingerprint, "id": readBack.calendarItemIdentifier]).write(to: receipt, options: [.atomic])
            return ["item": try record(readBack), "created": true, "replayed": false]
        }
        let item = try checkedItem(args, kind: kind)
        if action.hasSuffix("_delete") {
            let id = item.calendarItemIdentifier
            if let event = item as? EKEvent { try store.remove(event, span: .thisEvent, commit: true) }
            else if let reminder = item as? EKReminder { try store.remove(reminder, commit: true) }
            guard store.calendarItem(withIdentifier: id) == nil else { throw Failure("outcome_unknown", "删除后仍能读到记录，结果待核对") }
            return ["deleted_id": id, "verified_absent": true]
        }
        try setFields(item, args)
        try save(item)
        guard let readBack = store.calendarItem(withIdentifier: item.calendarItemIdentifier) else { throw Failure("outcome_unknown", "修改后未读回，结果待核对") }
        return ["item": try record(readBack)]
    }
}

@MainActor func selfTest() throws {
    let bad: [(String, [String: Any])] = [
        ("mac_calendar_create", ["title": "x", "start": "2026-10-16T15:00:00", "end": "2026-10-16T16:00:00Z", "request_id": "test-123"]),
        ("mac_calendar_list", ["start": "2026-10-16T16:00:00Z", "end": "2026-10-16T15:00:00Z"]),
        ("mac_reminder_create", ["title": "x", "request_id": "../../bad"]),
        ("mac_reminder_update", ["id": "x", "expected_revision": "v"]),
        ("mac_reminders_list", ["include_completed": 1]),
        ("mac_reminders_list", ["notes_prefix": NSNull()]),
        ("mac_status", ["execute": "anything"])
    ]
    for (action, args) in bad {
        do { try validate(action, args); throw Failure("test_failed", "非法参数未拒绝：\(action)") }
        catch let error as Failure where error.code == "bad_args" { }
    }
    try validate("mac_calendar_create", ["title": "中文 \\\" $()", "start": "2026-10-16T15:00:00+08:00", "end": "2026-10-16T16:00:00+08:00", "request_id": "test-123"])
    try validate("mac_reminder_update", ["id": "x", "expected_revision": "v", "due_at": NSNull(), "completed": true])
    guard try digest(["a": 1, "b": 2]) == digest(["b": 2, "a": 1]) else { throw Failure("test_failed", "版本摘要不稳定") }
    guard reminderMatches(notes: "day-1 task", completed: true, args: ["include_completed":true,"notes_prefix":"day-1"]),
          !reminderMatches(notes: "day-2 task", completed: false, args: ["notes_prefix":"day-1"]),
          !reminderMatches(notes: "day-1 task", completed: true, args: ["notes_prefix":"day-1"]) else { throw Failure("test_failed", "待办前缀或完成状态过滤错误") }
    print("native validation self-test: 13 passed")
    let definitions = try assistantDefinitions()
    guard definitions.count == 19 else { throw Failure("test_failed", "工具目录数目错误") }
    for definition in definitions {
        guard let schema = definition["parameters"] as? [String: Any] else { throw Failure("test_failed", "工具schema缺失") }
        do { try validateAssistant(["execute": "x"], schema); throw Failure("test_failed", "未知参数未拒绝") }
        catch let error as Failure where error.code == "bad_args" { }
    }
    let person = CNMutableContact(); person.givenName = "张"; person.familyName = "测试"; person.organizationName = "合成公司"; person.jobTitle = "经理"
    person.emailAddresses = [CNLabeledValue(label: CNLabelWork, value: "test@example.com" as NSString)]
    let contact = try AssistantBridge.contact(person)
    guard let emails = contact["emails"] as? [[String: String]], emails.first?["address"] == "test@example.com" else { throw Failure("test_failed", "联系人邮箱映射错误") }
    let temp = FileManager.default.temporaryDirectory.appendingPathComponent("yunxi-doc-test-" + UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: temp) }
    try String(repeating: "云", count: 8001).write(to: temp, atomically: true, encoding: .utf8)
    let first = try AssistantBridge.document(["path":temp.path]), last = try AssistantBridge.document(["path":temp.path,"offset":8000])
    guard first["next_offset"] as? Int == 8000, last["text"] as? String == "云", last["next_offset"] is NSNull else { throw Failure("test_failed", "文档分页错误") }
    try Data([0,1,2]).write(to: temp)
    do { _ = try AssistantBridge.document(["path":temp.path]); throw Failure("test_failed", "二进制附件未拒绝") }
    catch let error as Failure where error.code == "unsupported" { }
    print("assistant validation self-test: 23 passed")
}

@main struct Main {
    @MainActor static func main() {
        if CommandLine.arguments.contains("--self-test") {
            do { try selfTest(); exit(0) } catch { fputs("native self-test failed: \(error)\n", stderr); exit(1) }
        }
        // LaunchServices 派生的应用不在 open 的进程组中，必须自带有限生命周期。
        DispatchQueue.global().asyncAfter(deadline: .now() + 120) { exit(124) }
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        Task { @MainActor in
            var reply: [String: Any]
            do {
                let data = FileHandle.standardInput.readDataToEndOfFile()
                guard data.count <= 128_000, let request = try JSONSerialization.jsonObject(with: data) as? [String: Any], Set(request.keys) == ["action", "args"], let action = request["action"] as? String, let args = request["args"] as? [String: Any] else { throw Failure("bad_args", "无效请求") }
                if action != "authorize" { DispatchQueue.global().asyncAfter(deadline: .now() + 25) { exit(124) } }
                reply = ["ok": true, "result": try await Bridge().run(action, args)]
            } catch let error as Failure { reply = ["ok": false, "code": error.code, "message": error.message] }
            catch { reply = ["ok": false, "code": "system_error", "message": "macOS 操作失败（\((error as NSError).domain):\((error as NSError).code)）；未确认成功，请先查询核对。"] }
            do { FileHandle.standardOutput.write(try encoded(reply)) } catch { exit(1) }
            exit(0)
        }
        app.run()
    }
}
