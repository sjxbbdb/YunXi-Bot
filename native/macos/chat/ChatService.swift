import Foundation
import Combine

/// 一条聊天消息。
struct ChatMessage: Identifiable, Equatable {
    enum Role { case user, assistant, system }
    let id = UUID()
    let role: Role
    var text: String
    let at: Date?
}

/// 侧边栏里的一个会话。
struct ChatSession: Identifiable, Equatable {
    let id: String
    let preview: String
    let turns: Int
    let updatedAt: Double
    let hasHistory: Bool
}

/// 等待用户决定的工具审批。
struct PendingApproval: Identifiable, Equatable {
    let id: Int
    let tool: String
    let capability: String
    let specifier: String?
    let reason: String
    let preview: String?
    let arguments: String
    let requireHuman: Bool
}

/// 与 `yunxi-bot chat-server` 的 stdio 连接。
final class ChatService: ObservableObject {
    @Published var messages: [ChatMessage] = []
    @Published var sessions: [ChatSession] = []
    @Published var capabilities: Capabilities?
    @Published var pendingApproval: PendingApproval?
    @Published var status: String = "连接中…"
    @Published var connected = false
    @Published var busy = false
    @Published var session: String = ""
    @Published var modelInfo: String = ""
    @Published var effort: String = "auto"

    private var process: Process?
    private var stdinHandle: FileHandle?
    private var stdoutHandle: FileHandle?
    private var stderrHandle: FileHandle?
    private var readBuffer = Data()
    private var streamingID: UUID?
    private var started = false

    // MARK: - 生命周期

    func start() {
        guard !started else { return }
        started = true
        guard let repo = Self.findRepoRoot() else {
            fail("找不到项目目录（可用 YUNXI_REPO_ROOT 指定）")
            return
        }
        let bin = repo.appendingPathComponent("target/release/yunxi-bot")
        guard FileManager.default.isExecutableFile(atPath: bin.path) else {
            fail("找不到 \(bin.path)，先运行 cargo build --release")
            return
        }
        let dataDir = repo.appendingPathComponent("data")

        let p = Process()
        p.executableURL = bin
        p.arguments = ["chat-server", "--resume"]
        p.currentDirectoryURL = repo
        var env = ProcessInfo.processInfo.environment
        env["YUNXI_BOT_HOME"] = dataDir.path
        p.environment = env

        let inPipe = Pipe(), outPipe = Pipe(), errPipe = Pipe()
        p.standardInput = inPipe
        p.standardOutput = outPipe
        p.standardError = errPipe

        outPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            DispatchQueue.main.async { self?.consume(data) }
        }
        errPipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            if !data.isEmpty {
                FileHandle.standardError.write(data)
            }
        }
        p.terminationHandler = { [weak self] _ in
            DispatchQueue.main.async {
                self?.connected = false
                self?.busy = false
                self?.status = "后端已退出"
            }
        }

        do {
            try p.run()
            process = p
            stdinHandle = inPipe.fileHandleForWriting
            stdoutHandle = outPipe.fileHandleForReading
            stderrHandle = errPipe.fileHandleForReading
            status = "正在唤醒云熙…"
            DispatchQueue.main.asyncAfter(deadline: .now() + 8) { [weak self] in
                guard let self, !self.connected else { return }
                self.status = "等待系统权限：请允许访问「桌面」文件夹"
            }
        } catch {
            fail("启动失败：\(error.localizedDescription)")
        }
    }

    func stop() {
        stdoutHandle?.readabilityHandler = nil
        stderrHandle?.readabilityHandler = nil
        try? stdinHandle?.close()
        process?.terminate()
        process = nil
    }

    // MARK: - 发送

    func send(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, connected, !busy else { return }
        messages.append(ChatMessage(role: .user, text: trimmed, at: Date()))
        busy = true
        write(["type": "chat", "text": trimmed])
    }

    func newChat() {
        guard connected, !busy else { return }
        write(["type": "new"])
    }

    func openSession(_ id: String) {
        guard connected, !busy, id != session else { return }
        messages.removeAll()
        pendingApproval = nil
        streamingID = nil
        busy = true
        write(["type": "open", "session": id])
    }

    func requestSessions() {
        guard connected else { return }
        write(["type": "list_sessions"])
    }

    func requestCapabilities() {
        guard connected else { return }
        write(["type": "capabilities"])
    }

    func setEffort(_ value: String) {
        guard connected else { return }
        effort = value
        write(["type": "effort", "value": value])
    }

    func respondApproval(_ id: Int, allow: Bool) {
        pendingApproval = nil
        write(["type": "approval", "id": id, "allow": allow])
    }

    private func write(_ object: [String: Any]) {
        guard let handle = stdinHandle,
              let data = try? JSONSerialization.data(withJSONObject: object),
              var line = String(data: data, encoding: .utf8) else {
            fail("无法写入后端")
            return
        }
        line += "\n"
        do {
            try handle.write(contentsOf: Data(line.utf8))
        } catch {
            fail("写入失败：\(error.localizedDescription)")
        }
    }

    // MARK: - 接收

    private func consume(_ data: Data) {
        readBuffer.append(data)
        while let newline = readBuffer.firstIndex(of: 0x0A) {
            let lineData = readBuffer.subdata(in: readBuffer.startIndex..<newline)
            readBuffer.removeSubrange(readBuffer.startIndex...newline)
            if !lineData.isEmpty { handleLine(lineData) }
        }
    }

    private func handleLine(_ data: Data) {
        guard let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = object["type"] as? String else { return }
        switch type {
        case "ready":
            connected = true
            busy = false
            session = object["session"] as? String ?? ""
            status = "在线"
            requestSessions()
        case "sessions":
            applySessions(object["items"] as? [[String: Any]] ?? [])
        case "capabilities":
            if let snapshot = object["snapshot"] as? [String: Any],
               let decoded = Capabilities.decode(from: snapshot) {
                capabilities = decoded
            }
        case "history":
            applyHistory(object["messages"] as? [[String: Any]] ?? [])
        case "opened":
            busy = false
            session = object["session"] as? String ?? session
            let turns = object["turns"] as? Int ?? 0
            if messages.isEmpty {
                let missing = object["history_missing"] as? Bool ?? false
                if missing {
                    messages.append(ChatMessage(role: .system, text: "旧会话没有可回显的显示记录（\(turns) 轮），上下文已恢复。", at: nil))
                }
            }
        case "approval":
            applyApproval(object)
        case "approval_expired":
            if let id = object["id"] as? Int, pendingApproval?.id == id {
                pendingApproval = nil
                messages.append(ChatMessage(role: .system, text: "审批等待超时，已按拒绝处理。", at: nil))
            }
        case "effort":
            effort = object["value"] as? String ?? effort
        case "delta":
            if let text = object["text"] as? String {
                appendDelta(text)
            }
        case "route":
            let model = object["model"] as? String ?? ""
            let thinking = object["thinking"] as? Bool ?? false
            modelInfo = model + (thinking ? " · 思考" : "")
        case "reply":
            finishReply(object["text"] as? String ?? "")
        case "error":
            busy = false
            streamingID = nil
            pendingApproval = nil
            messages.append(ChatMessage(role: .system, text: "⚠️ " + (object["message"] as? String ?? "未知错误"), at: nil))
        case "session":
            session = object["session"] as? String ?? ""
            messages.removeAll()
            streamingID = nil
            pendingApproval = nil
            busy = false
            requestSessions()
        default:
            break
        }
    }

    private func applySessions(_ items: [[String: Any]]) {
        sessions = items.compactMap { item in
            guard let id = item["id"] as? String else { return nil }
            return ChatSession(
                id: id,
                preview: item["preview"] as? String ?? "",
                turns: item["turns"] as? Int ?? 0,
                updatedAt: item["updated_at"] as? Double ?? 0,
                hasHistory: item["has_history"] as? Bool ?? false
            )
        }
    }

    private func applyHistory(_ rows: [[String: Any]]) {
        for row in rows {
            guard let role = row["role"] as? String, let text = row["text"] as? String else { continue }
            let kind: ChatMessage.Role = role == "user" ? .user : .assistant
            messages.append(ChatMessage(role: kind, text: text, at: nil))
        }
    }

    private func applyApproval(_ object: [String: Any]) {
        guard let id = object["id"] as? Int else { return }
        pendingApproval = PendingApproval(
            id: id,
            tool: object["tool"] as? String ?? "?",
            capability: object["capability"] as? String ?? "",
            specifier: object["specifier"] as? String,
            reason: object["reason"] as? String ?? "",
            preview: object["preview"] as? String,
            arguments: object["arguments"] as? String ?? "",
            requireHuman: object["require_human"] as? Bool ?? true
        )
    }

    private func appendDelta(_ text: String) {
        if let id = streamingID, let index = messages.firstIndex(where: { $0.id == id }) {
            messages[index].text += text
        } else {
            let message = ChatMessage(role: .assistant, text: text, at: Date())
            streamingID = message.id
            messages.append(message)
        }
    }

    private func finishReply(_ text: String) {
        if let id = streamingID, let index = messages.firstIndex(where: { $0.id == id }) {
            messages[index].text = text
        } else {
            messages.append(ChatMessage(role: .assistant, text: text, at: Date()))
        }
        streamingID = nil
        busy = false
    }

    private func fail(_ message: String) {
        status = "离线"
        connected = false
        messages.append(ChatMessage(role: .system, text: "⚠️ " + message, at: nil))
    }

    // MARK: - 找项目根目录

    static func findRepoRoot() -> URL? {
        if let custom = ProcessInfo.processInfo.environment["YUNXI_REPO_ROOT"], !custom.isEmpty {
            return URL(fileURLWithPath: custom)
        }
        var url = URL(fileURLWithPath: Bundle.main.bundlePath)
        for _ in 0..<10 {
            url.deleteLastPathComponent()
            let bin = url.appendingPathComponent("target/release/yunxi-bot")
            if FileManager.default.isExecutableFile(atPath: bin.path) {
                return url
            }
            if url.path == "/" { break }
        }
        return nil
    }
}
