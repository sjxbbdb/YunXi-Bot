// Cua embedding protocol: third_party/cua/libs/cua-driver/rust/Skills/cua-driver/EMBEDDING.md.
import AppKit
import ApplicationServices
import CoreGraphics
import Darwin

private let hostID = "local.yunxi.bot.computer"
private let root = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent("Library/Application Support/YunXiBot/computer", isDirectory: true)

private func privateDirectory() throws {
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                                            attributes: [.posixPermissions: 0o700])
    var info = stat()
    guard lstat(root.path, &info) == 0, info.st_mode & S_IFMT == S_IFDIR,
          info.st_uid == getuid(), info.st_mode & 0o077 == 0 else {
        throw NSError(domain: hostID, code: 1)
    }
}

private func childEnvironment() -> [String: String] {
    let inherited = ProcessInfo.processInfo.environment
    var env = inherited.filter { ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG"].contains($0.key) }
    env["PATH"] = "/usr/bin:/bin:/usr/sbin:/sbin"
    env["CUA_DRIVER_EMBEDDED"] = "1"
    env["CUA_DRIVER_HOST_BUNDLE_ID"] = hostID
    env["CUA_DRIVER_RS_HOME"] = root.appendingPathComponent("state").path
    env["CUA_DRIVER_RS_TELEMETRY_ENABLED"] = "false"
    env["CUA_TELEMETRY_ENABLED"] = "false"
    env["CUA_DRIVER_RS_UPDATE_CHECK"] = "false"
    return env
}

private func socketIsLive(_ path: String) throws -> Bool {
    var address = sockaddr_un()
    address.sun_family = sa_family_t(AF_UNIX)
    let bytes = Array(path.utf8) + [0]
    guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else { throw NSError(domain: hostID, code: 8) }
    withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: bytes) }
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { throw NSError(domain: hostID, code: 9) }
    defer { Darwin.close(fd) }
    guard fcntl(fd, F_SETFL, O_NONBLOCK) == 0 else { throw NSError(domain: hostID, code: 10) }
    let result = withUnsafePointer(to: &address) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
    }
    if result == 0 || errno == EINPROGRESS { return true }
    if errno == ECONNREFUSED || errno == ENOENT { return false }
    throw NSError(domain: hostID, code: 11)
}

@MainActor final class ComputerHost: NSObject, NSApplicationDelegate {
    private var child: Process?
    private var liveness: Pipe?
    private var lock: Int32 = -1
    private var signals: [DispatchSourceSignal] = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        do {
            guard Bundle.main.bundleIdentifier == hostID else { throw NSError(domain: hostID, code: 2) }
            if CommandLine.arguments.contains("--authorize") {
                _ = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary)
                _ = CGRequestScreenCaptureAccess()
                NSApp.terminate(nil)
                return
            }
            try privateDirectory()
            lock = Darwin.open(root.appendingPathComponent("host.lock").path,
                               O_CREAT | O_RDWR | O_NOFOLLOW | O_CLOEXEC, 0o600)
            guard lock >= 0, flock(lock, LOCK_EX | LOCK_NB) == 0 else {
                throw NSError(domain: hostID, code: 3)
            }
            let socket = root.appendingPathComponent("driver.sock")
            // Only remove an owned stale socket while holding the singleton host lock.
            var info = stat()
            if lstat(socket.path, &info) == 0 {
                guard info.st_mode & S_IFMT == S_IFSOCK, info.st_uid == getuid() else {
                    throw NSError(domain: hostID, code: 4)
                }
                // A previous child may still be shutting down after its host died.
                guard try !socketIsLive(socket.path) else { throw NSError(domain: hostID, code: 6) }
                try FileManager.default.removeItem(at: socket)
            }
            let process = Process()
            process.executableURL = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/cua-driver")
            var args = ["serve", "--embedded", "--parent-liveness-stdio", "--no-permissions-gate",
                        "--permission-mode", "standard", "--socket", socket.path, "--host-bundle-id", hostID]
            let grant = root.appendingPathComponent("existing-profile.allowed")
            if lstat(grant.path, &info) == 0 {
                guard info.st_mode & S_IFMT == S_IFREG, info.st_uid == getuid(),
                      info.st_mode & 0o077 == 0 else { throw NSError(domain: hostID, code: 7) }
                if try String(contentsOf: grant, encoding: .utf8) == "allowed\n" { args += ["--grant", "existing-profile"] }
            }
            process.arguments = args
            process.environment = childEnvironment()
            let pipe = Pipe()
            process.standardInput = pipe
            // Raw Driver output can include private interface data; do not persist it.
            process.standardOutput = FileHandle.nullDevice
            process.standardError = FileHandle.nullDevice
            process.terminationHandler = { _ in DispatchQueue.main.async { NSApp.terminate(nil) } }
            try process.run()
            pipe.fileHandleForReading.closeFile()
            child = process
            liveness = pipe
            for sig in [SIGTERM, SIGINT] {
                signal(sig, SIG_IGN)
                let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
                source.setEventHandler { NSApp.terminate(nil) }
                source.resume()
                signals.append(source)
            }
        } catch {
            FileHandle.standardError.write(Data("computer_host_unavailable: startup refused; no action replayed\n".utf8))
            NSApp.terminate(nil)
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        child?.terminationHandler = nil
        liveness?.fileHandleForWriting.closeFile()
        if let child, child.isRunning {
            child.terminate()
            let until = Date().addingTimeInterval(2)
            while child.isRunning && Date() < until { Thread.sleep(forTimeInterval: 0.05) }
            if child.isRunning { kill(child.processIdentifier, SIGKILL) }
        }
        if lock >= 0 { Darwin.close(lock) }
    }
}

@main struct Main {
    @MainActor static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        let delegate = ComputerHost()
        app.delegate = delegate
        withExtendedLifetime(delegate) { app.run() }
    }
}
