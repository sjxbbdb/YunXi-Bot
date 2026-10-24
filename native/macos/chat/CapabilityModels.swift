import Foundation

// MARK: - 能力快照（与 chat-server 的 capabilities 事件一一对应）

struct Capabilities: Codable {
    let generatedAt: Double
    let version: String
    let home: String
    let platform: String
    let bot: BotInfo
    let identity: IdentityInfo
    let tools: ToolsInfo
    let memory: MemoryInfo
    let tasks: TasksInfo
    let ledger: LedgerInfo
    let cost: CostInfo
    let system: SystemInfo

    static func decode(from object: [String: Any]) -> Capabilities? {
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object) else {
            return nil
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let value = try? decoder.decode(Capabilities.self, from: data) else {
            return nil
        }
        return value
    }
}

struct BotInfo: Codable {
    let model: String
    let effort: String
    let session: String
    let turns: Int
    let sessionCount: Int
    let latestSession: String?
    let latestTurns: Int?
}

struct IdentityInfo: Codable {
    let personaBytes: Int
    let profileBytes: Int
}

struct LabelCount: Codable, Identifiable {
    var id: String { label }
    let label: String
    let count: Int
}

struct ToolItem: Codable, Identifiable {
    var id: String { name }
    let name: String
    let description: String
    let capability: String
    let approval: String
    let privateIo: Bool
    let freshApproval: Bool
}

struct ToolsInfo: Codable {
    let total: Int
    let byCapability: [LabelCount]
    let items: [ToolItem]
    let mcpCount: Int
    let mcpProblems: [String]
}

struct MemoryItem: Codable, Identifiable {
    let id: String
    let kind: String
    let text: String
    let weight: Double
    let createdAt: Double
}

struct PendingItem: Codable, Identifiable {
    let id: String
    let kind: String
    let text: String
    let quote: String
    let sourceSeq: Int
}

struct MemoryInfo: Codable {
    let total: Int
    let byKind: [LabelCount]
    let recent: [MemoryItem]
    let pending: [PendingItem]
    let pendingTotal: Int
    let followups: Int
    let knowledgeDocuments: Int
}

struct JobItem: Codable, Identifiable {
    let id: String
    let name: String
    let command: String
    let trigger: String
    let state: String
    let irreversible: Bool
    let lastRunAt: Double?
    let lastError: String?
}

struct DoTaskItem: Codable, Identifiable {
    let id: String
    let goal: String
    let state: String
    let steps: Int
    let done: Int
    let failed: Int
    let skipped: Int
}

struct TasksInfo: Codable {
    let jobs: [JobItem]
    let doTasks: [DoTaskItem]
}

struct RecentEvent: Codable, Identifiable {
    var id: Int { seq }
    let seq: Int
    let kind: String
    let at: Double
}

struct KindCount: Codable, Identifiable {
    var id: String { kind }
    let kind: String
    let count: Int
}

struct LedgerInfo: Codable {
    let events: Int
    let recent: [RecentEvent]
    let topKinds: [KindCount]
    let bytes: Int
}

struct CostInfo: Codable {
    let calls: Int
    let promptTokens: Int
    let completionTokens: Int
    let cacheHitTokens: Int
    let cacheMissTokens: Int
    let cacheHitRate: Double?
    let thinkingCalls: Int
    let total: Double
    let saved: Double
    let currency: String
}

struct DaemonInfo: Codable {
    let pid: Int?
    let alive: Bool
    let lockPresent: Bool
}

struct AutostartInfo: Codable {
    let registered: Bool
    let path: String?
}

struct ServiceInfo: Codable, Identifiable {
    var id: Int { port }
    let name: String
    let port: Int
    let listening: Bool
}

struct CompanionInfo: Codable {
    let localListening: Bool
    let localModelState: String
    let lastMemoryAt: Double?
    let lastKnowledgeAt: Double?
    let pendingMemories: Int
}

struct SystemInfo: Codable {
    let companion: CompanionInfo?
    let daemon: DaemonInfo
    let autostart: AutostartInfo
    let services: [ServiceInfo]
    let models: [String]
    let chatUiSessions: Int
    let mailConfigured: Bool
    let ledgerPath: String
}
