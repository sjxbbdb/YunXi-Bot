import AppKit
import Contacts
import MapKit
import OSAKit
import PDFKit

func assistantDefinitions() throws -> [[String: Any]] {
    guard let url = Bundle.main.url(forResource: "assistant-tools", withExtension: "json"),
          let definitions = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]] else {
        throw Failure("installation_error", "第二批工具目录缺失，请重建原生应用")
    }
    return definitions
}

func validateAssistant(_ value: Any, _ schema: [String: Any], _ field: String = "参数") throws {
    if let choices = schema["enum"] as? [String], let s = value as? String, !choices.contains(s) {
        throw Failure("bad_args", "\(field) 不在允许的选项内")
    }
    var valid = false
    switch schema["type"] as? String {
    case "object":
        guard let object = value as? [String: Any], let properties = schema["properties"] as? [String: [String: Any]],
              Set(object.keys).isSubset(of: Set(properties.keys)), Set(schema["required"] as? [String] ?? []).isSubset(of: Set(object.keys)) else {
            throw Failure("bad_args", "\(field) 缺少必填字段或含未知字段")
        }
        for (key, item) in object { if let property = properties[key] { try validateAssistant(item, property, "\(field).\(key)") } }
        valid = true
    case "array":
        if let array = value as? [Any], let item = schema["items"] as? [String: Any], array.count <= (schema["maxItems"] as? Int ?? 0) {
            for v in array { try validateAssistant(v, item, field) }
            valid = true
        }
    case "string":
        if let s = value as? String {
            valid = !s.contains("\0") && s.utf8.count <= (schema["maxLength"] as? Int ?? 200)
                && ((schema["minLength"] as? Int ?? 0) == 0 || !s.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
    case "boolean":
        if let n = value as? NSNumber { valid = CFGetTypeID(n) == CFBooleanGetTypeID() }
    case "integer", "number":
        if let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID() {
            let d = n.doubleValue
            valid = d.isFinite && d >= (schema["minimum"] as? Double ?? .infinity) && d <= (schema["maximum"] as? Double ?? -.infinity)
                && (schema["type"] as? String != "integer" || d.rounded() == d)
        }
    default: break
    }
    guard valid else { throw Failure("bad_args", "\(field) 类型或范围不合法") }
}

func nativeWrite<T>(_ body: (URL) throws -> T) throws -> T {
    let state = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/YunXiBot/native", isDirectory: true)
    try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    let lock = Darwin.open(state.appendingPathComponent("write.lock").path, O_CREAT | O_RDWR, 0o600)
    guard lock >= 0 else { throw Failure("state_failed", "无法打开写入锁") }
    defer { Darwin.close(lock) }
    guard flock(lock, LOCK_EX | LOCK_NB) == 0 else { throw Failure("busy", "另一项本机写入正在进行，请稍后重新读取") }
    defer { flock(lock, LOCK_UN) }
    return try body(state)
}

@MainActor enum AssistantBridge {
    static func contactsStatus() -> String {
        switch CNContactStore.authorizationStatus(for: .contacts) {
        case .authorized: return "full_access"
        case .limited: return "limited"
        case .notDetermined: return "not_determined"
        case .restricted: return "restricted"
        case .denied: return "denied"
        @unknown default: return "unavailable"
        }
    }

    static func mailPermission(ask: Bool) async -> String {
        guard !NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.mail").isEmpty else { return "mail_not_running" }
        let code: OSStatus = await withCheckedContinuation { continuation in
            DispatchQueue.global().async {
                let target = NSAppleEventDescriptor(bundleIdentifier: "com.apple.mail")
                continuation.resume(returning: AEDeterminePermissionToAutomateTarget(target.aeDesc, typeWildCard, typeWildCard, ask))
            }
        }
        switch Int(code) {
        case 0: return "full_access"
        case errAEEventWouldRequireUserConsent: return "not_determined"
        case errAEEventNotPermitted: return "denied"
        default: return "unavailable"
        }
    }

    static func startMail() async throws {
        if NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.mail").isEmpty {
            let configuration = NSWorkspace.OpenConfiguration()
            configuration.activates = false
            _ = try await NSWorkspace.shared.openApplication(at: URL(fileURLWithPath: "/System/Applications/Mail.app"), configuration: configuration)
        }
    }

    static func authorize(_ target: String) async throws -> [String: Any] {
        if target == "contacts" {
            if CNContactStore.authorizationStatus(for: .contacts) == .notDetermined { _ = try await CNContactStore().requestAccess(for: .contacts) }
            return ["contacts": contactsStatus()]
        }
        try await startMail()
        return ["mail": await mailPermission(ask: true)]
    }

    static func contact(_ contact: CNContact) throws -> [String: Any] {
        var result: [String: Any] = ["id": contact.identifier, "name": CNContactFormatter.string(from: contact, style: .fullName) ?? "",
                                   "nickname": contact.nickname, "company": contact.organizationName, "job_title": contact.jobTitle,
                                   "emails": contact.emailAddresses.map { ["address": $0.value as String, "label": $0.label.map { CNLabeledValue<NSString>.localizedString(forLabel: $0) } ?? ""] }]
        result["revision"] = try digest(result)
        return result
    }

    static func contacts(_ action: String, _ args: [String: Any]) throws -> [String: Any] {
        let status = contactsStatus()
        guard ["full_access", "limited"].contains(status) else { throw Failure("permission_required", "请先运行 mac authorize contacts 并在系统弹窗允许；未读取通讯录") }
        let keys: [CNKeyDescriptor] = [CNContactFormatter.descriptorForRequiredKeys(for: .fullName), CNContactIdentifierKey as CNKeyDescriptor,
                                      CNContactNicknameKey as CNKeyDescriptor, CNContactOrganizationNameKey as CNKeyDescriptor,
                                      CNContactJobTitleKey as CNKeyDescriptor, CNContactEmailAddressesKey as CNKeyDescriptor]
        let store = CNContactStore()
        if action == "mac_contact_get" { return try contact(store.unifiedContact(withIdentifier: text(args, "id"), keysToFetch: keys)) }
        let query = try text(args, "query")
        var items: [[String: Any]] = [], scanned = 0, truncated = false
        var contactError: Error?
        let request = CNContactFetchRequest(keysToFetch: keys)
        try store.enumerateContacts(with: request) { c, stop in
            scanned += 1
            if scanned > 5000 { truncated = true; stop.pointee = true; return }
            let name = CNContactFormatter.string(from: c, style: .fullName) ?? ""
            let searchable = [name, c.nickname, c.organizationName, c.jobTitle] + c.emailAddresses.map { $0.value as String }
            if searchable.contains(where: { $0.localizedStandardContains(query) }) {
                if items.count == 20 { truncated = true; stop.pointee = true; return }
                do { items.append(try contact(c)) }
                catch { contactError = error; stop.pointee = true }
            }
        }
        if let contactError { throw contactError }
        return ["items": items, "truncated": truncated, "ambiguous": items.count > 1,
                "access": status, "degraded": status == "limited", "reason": status == "limited" ? "系统仅允许读取部分联系人" : ""]
    }

    static func mapItem(_ latitude: Double, _ longitude: Double) -> MKMapItem {
        if #available(macOS 26.0, *) { return MKMapItem(location: CLLocation(latitude: latitude, longitude: longitude), address: nil) }
        return MKMapItem(placemark: MKPlacemark(coordinate: CLLocationCoordinate2D(latitude: latitude, longitude: longitude)))
    }

    static func places(_ args: [String: Any]) async throws -> [String: Any] {
        let request = MKLocalSearch.Request()
        request.naturalLanguageQuery = try text(args, "query", max: 500)
        let response = try await MKLocalSearch(request: request).start()
        let items: [[String: Any]] = response.mapItems.prefix(10).map { item in
            let coordinate: CLLocationCoordinate2D, address: String
            if #available(macOS 26.0, *) { coordinate = item.location.coordinate; address = item.address?.fullAddress ?? "" }
            else { coordinate = item.placemark.coordinate; address = item.placemark.title ?? "" }
            return ["name": item.name ?? "", "address": address, "latitude": coordinate.latitude, "longitude": coordinate.longitude,
                    "website": item.url?.absoluteString as Any? ?? NSNull(), "phone": item.phoneNumber as Any? ?? NSNull(),
                    "time_zone": item.timeZone?.identifier as Any? ?? NSNull(), "opening_hours": NSNull()]
        }
        return ["items": items, "truncated": response.mapItems.count > 10, "source": "Apple Maps",
                "fetched_at": stamp(Date()), "opening_hours_status": "not_provided_by_api; verify merchant website with web_search/web_fetch"]
    }

    static func route(_ args: [String: Any]) async throws -> [String: Any] {
        guard let a = args["source_latitude"] as? Double, let b = args["source_longitude"] as? Double,
              let c = args["destination_latitude"] as? Double, let d = args["destination_longitude"] as? Double else { throw Failure("bad_args", "缺少起终点坐标") }
        let request = MKDirections.Request()
        request.source = mapItem(a, b); request.destination = mapItem(c, d)
        let mode = try text(args, "mode")
        request.transportType = mode == "walking" ? .walking : mode == "transit" ? .transit : .automobile
        let arrival = try date(text(args, "arrive_at"))
        guard arrival > Date() else { throw Failure("bad_args", "到达时间必须在未来") }
        request.arrivalDate = arrival
        let eta = try await MKDirections(request: request).calculateETA()
        guard eta.expectedTravelTime.isFinite, eta.expectedTravelTime > 0, eta.expectedTravelTime <= 86400 else { throw Failure("invalid_response", "地图返回无效通勤时间") }
        var out: [String: Any] = ["source": "Apple Maps", "mode": mode, "travel_minutes": Int(ceil(eta.expectedTravelTime / 60)), "distance_meters": eta.distance,
                                 "departure_at": stamp(eta.expectedDepartureDate), "arrival_at": stamp(eta.expectedArrivalDate), "requested_arrival_at": stamp(arrival),
                                 "estimated": true, "fetched_at": stamp(Date()),
                                 "maps_url": "https://maps.apple.com/?saddr=\(a),\(b)&daddr=\(c),\(d)&dirflg=\(mode == "walking" ? "w" : mode == "transit" ? "r" : "d")"]
        if mode == "transit" {
            out["steps"] = []; out["steps_status"] = "换乘详情需在Apple地图确认；此接口只返回公交预计耗时"
        } else {
            let response = try await MKDirections(request: request).calculate()
            guard let route = response.routes.min(by: { $0.expectedTravelTime < $1.expectedTravelTime }) else { throw Failure("unavailable", "地图没有返回路线") }
            out["route_name"] = route.name; out["advisories"] = route.advisoryNotices
            out["steps"] = route.steps.map { ["instruction": $0.instructions, "distance_meters": $0.distance] as [String: Any] }
        }
        return out
    }

    static func weather(_ args: [String: Any]) async throws -> [String: Any] {
        guard let latitude = args["latitude"] as? Double, let longitude = args["longitude"] as? Double else { throw Failure("bad_args", "坐标缺失") }
        let day = try text(args, "date", max: 10)
        let formatter = DateFormatter(); formatter.locale = Locale(identifier: "en_US_POSIX"); formatter.dateFormat = "yyyy-MM-dd"; formatter.isLenient = false
        guard let date = formatter.date(from: day), formatter.string(from: date) == day,
              let delta = Calendar.current.dateComponents([.day], from: Calendar.current.startOfDay(for: Date()), to: date).day,
              (0...15).contains(delta) else { throw Failure("bad_args", "天气日期必须在今天起16天内，格式YYYY-MM-DD") }
        var url = URLComponents(string: "https://api.open-meteo.com/v1/forecast")
        url?.queryItems = [URLQueryItem(name: "latitude", value: String(latitude)), URLQueryItem(name: "longitude", value: String(longitude)),
                           URLQueryItem(name: "start_date", value: day), URLQueryItem(name: "end_date", value: day), URLQueryItem(name: "timezone", value: "auto"),
                           URLQueryItem(name: "hourly", value: "temperature_2m,precipitation_probability,weather_code,wind_speed_10m")]
        guard let endpoint = url?.url else { throw Failure("bad_args", "天气请求地址无效") }
        var request = URLRequest(url: endpoint); request.timeoutInterval = 18
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let response = response as? HTTPURLResponse, response.statusCode == 200, data.count <= 100_000,
              let result = try JSONSerialization.jsonObject(with: data) as? [String: Any], let hours = result["hourly"] as? [String: Any],
              let times = hours["time"] as? [String], !times.isEmpty, times.count <= 25 else { throw Failure("invalid_response", "天气服务没有返回完整有效的小时预报") }
        for field in ["temperature_2m", "precipitation_probability", "weather_code", "wind_speed_10m"] {
            guard let values = hours[field] as? [Any], values.count == times.count, values.allSatisfy({ $0 is NSNumber }) else { throw Failure("invalid_response", "天气预报字段缺失，不能用空值伪装正常预报") }
        }
        return ["source": "Open-Meteo", "source_url": endpoint.absoluteString, "fetched_at": stamp(Date()), "forecast": result,
                "weather_code_reference": "https://open-meteo.com/en/docs#weather_variable_documentation"]
    }

    static func document(_ args: [String: Any]) throws -> [String: Any] {
        let url = URL(fileURLWithPath: try text(args, "path", max: 1024))
        let info = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey, .isSymbolicLinkKey])
        guard info.isRegularFile == true, info.isSymbolicLink != true, let size = info.fileSize, size <= 20_000_000 else { throw Failure("bad_args", "只读取20MB以内的普通文件，不接受符号链接") }
        let content: String
        if url.pathExtension.lowercased() == "pdf" {
            guard let document = PDFDocument(url: url), !document.isLocked else { throw Failure("unsupported", "PDF无法打开或已加密") }
            var parts: [String] = [], count = 0
            for index in 0..<document.pageCount {
                let s = document.page(at: index)?.string ?? ""
                count += s.utf8.count
                guard count <= 2_000_000 else { throw Failure("too_large", "PDF提取文本超过2MB，请拆分文档") }
                parts.append(s)
            }
            content = parts.joined(separator: "\n")
        } else {
            let data = try Data(contentsOf: url)
            guard let s = String(data: data, encoding: .utf8), !s.contains("\0") else { throw Failure("unsupported", "当前文本提取支持UTF-8文本和PDF；不把二进制附件当正文") }
            guard s.utf8.count <= 2_000_000 else { throw Failure("too_large", "文本超过2MB，请拆分文档") }
            content = s
        }
        let offset = args["offset"] as? Int ?? 0, characters = Array(content)
        guard offset <= characters.count else { throw Failure("bad_args", "offset超出文本长度") }
        let end = min(offset + 8000, characters.count)
        return ["path": url.path, "text": String(characters[offset..<end]), "offset": offset, "next_offset": end < characters.count ? end as Any : NSNull(),
                "total_characters": characters.count, "text_extraction_empty": characters.isEmpty,
                "note": characters.isEmpty ? "未提取到文字；扫描PDF需OCR，不能声称已读懂内容" : ""]
    }

    static func attachmentFile(_ path: String) throws -> URL {
        let url = URL(fileURLWithPath: path)
        let info = try url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey, .isSymbolicLinkKey])
        guard path.hasPrefix("/"), info.isRegularFile == true, info.isSymbolicLink != true,
              let size = info.fileSize, size <= 20_000_000 else { throw Failure("bad_args", "附件必须是20MB以内的普通绝对路径文件") }
        return url
    }

    static func mailScript(_ action: String, _ args: [String: Any], state: URL) throws -> [String: Any] {
        guard let url = Bundle.main.url(forResource: "Mail", withExtension: "js"), let language = OSALanguage(forName: "JavaScript") else { throw Failure("installation_error", "邮件脚本或系统JavaScript自动化不可用") }
        let source = try String(contentsOf: url, encoding: .utf8)
        for path in args["attachments"] as? [String] ?? [] { _ = try attachmentFile(path) }
        let staging = FileManager.default.temporaryDirectory.appendingPathComponent("yunxi-mail-" + UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: staging) }
        var internalRequest: [String: Any] = ["action": action, "args": args, "state_path": state.path]
        if action == "mac_mail_attachment_save" {
            let destination = try text(args, "path", max: 1024)
            guard destination.hasPrefix("/"), !FileManager.default.fileExists(atPath: destination) else { throw Failure("conflict", "附件保存路径必须为新的绝对路径文件，不能覆盖已有文件") }
            try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
            internalRequest["attachment_staging"] = staging.appendingPathComponent("attachment").path
        }
        let request = try encoded(internalRequest)
        guard let payload = String(data: request, encoding: .utf8) else { throw Failure("bad_args", "请求编码失败") }
        let script = OSAScript(source: source + "\nJSON.stringify(runRequest(" + payload + "));", language: language)
        var error: NSDictionary?
        guard let output = script.executeAndReturnError(&error)?.stringValue, let data = output.data(using: .utf8),
              let result = try JSONSerialization.jsonObject(with: data) as? [String: Any] else { throw Failure("mail_error", "Mail自动化执行失败；结果未知，先核对，不能重复发送") }
        guard result["ok"] as? Bool == true, let value = result["result"] as? [String: Any] else {
            throw Failure(result["code"] as? String ?? "mail_error", result["message"] as? String ?? "邮件操作没有成功回执")
        }
        if action == "mac_mail_attachment_save" {
            guard value["staged"] as? Bool == true else { throw Failure("outcome_unknown", "附件未完成暂存") }
            let file = try attachmentFile(staging.appendingPathComponent("attachment").path)
            let data = try Data(contentsOf: file)
            let path = try text(args, "path", max: 1024)
            let fd = Darwin.open(path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600)
            guard fd >= 0 else { throw Failure("conflict", "目标已存在或不能写入；未覆盖文件") }
            let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
            do { try handle.write(contentsOf: data); try handle.synchronize(); try handle.close() }
            catch { try? FileManager.default.removeItem(atPath: path); throw error }
            return ["saved": true, "path": path, "size_bytes": data.count, "attachment_id": args["attachment_id"] as Any]
        }
        return value
    }

    static func run(_ action: String, _ args: [String: Any], definition: [String: Any]) async throws -> [String: Any] {
        guard let schema = definition["parameters"] as? [String: Any] else { throw Failure("installation_error", "工具缺少参数定义") }
        try validateAssistant(args, schema)
        switch action {
        case "mac_contacts_search", "mac_contact_get": return try contacts(action, args)
        case "mac_places_search": return try await places(args)
        case "mac_route": return try await route(args)
        case "mac_weather": return try await weather(args)
        case "mac_document_read": return try document(args)
        default:
            guard action.hasPrefix("mac_mail_") else { throw Failure("bad_args", "未知工具") }
            if !["mac_mail_outbox", "mac_mail_draft_get", "mac_mail_draft_update", "mac_mail_draft_discard"].contains(action) {
                try await startMail()
                guard await mailPermission(ask: false) == "full_access" else { throw Failure("permission_required", "请先运行 mac authorize mail 并允许云熙控制Mail；未读取或修改邮件") }
            }
            if definition["capability"] as? String == "outbound" { return try nativeWrite { try mailScript(action, args, state: $0) } }
            let state = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/YunXiBot/native")
            return try mailScript(action, args, state: state)
        }
    }
}
