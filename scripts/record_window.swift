import Foundation
import ScreenCaptureKit
import AVFoundation
import AppKit

_ = NSApplication.shared

final class RecorderDelegate: NSObject, SCRecordingOutputDelegate, @unchecked Sendable {
    private let lock = NSLock()
    private var status = "starting"
    func recordingOutputDidStartRecording(_ recordingOutput: SCRecordingOutput) { set("ready") }
    func recordingOutputDidFinishRecording(_ recordingOutput: SCRecordingOutput) { set("finished") }
    func recordingOutput(_ recordingOutput: SCRecordingOutput, didFailWithError error: Error) { set("failed") }
    func set(_ value: String) { lock.lock(); status=value; lock.unlock() }
    func get() -> String { lock.lock(); defer { lock.unlock() }; return status }
}

guard CommandLine.arguments.count == 3, let windowID = UInt32(CommandLine.arguments[1]) else { throw NSError(domain:"arguments",code:1) }
let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
guard let window = content.windows.first(where: { $0.windowID == windowID }) else { throw NSError(domain:"window-not-found",code:1) }
let config = SCStreamConfiguration()
config.width = Int(window.frame.width) * 2
config.height = Int(window.frame.height) * 2
config.minimumFrameInterval = CMTime(value:1,timescale:15)
config.capturesAudio = false
config.captureMicrophone = false
config.showsCursor = true
let outputConfig = SCRecordingOutputConfiguration()
outputConfig.outputURL = URL(fileURLWithPath: CommandLine.arguments[2])
outputConfig.outputFileType = .mp4
outputConfig.videoCodecType = .h264
let delegate = RecorderDelegate()
let output = SCRecordingOutput(configuration:outputConfig,delegate:delegate)
let stream = SCStream(filter:SCContentFilter(desktopIndependentWindow:window),configuration:config,delegate:nil)
try stream.addRecordingOutput(output)
try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
    stream.startCapture { error in if let error { c.resume(throwing:error) } else { c.resume() } }
}
for _ in 0..<100 { if delegate.get() != "starting" { break }; try await Task.sleep(for:.milliseconds(100)) }
guard delegate.get() == "ready" else { throw NSError(domain:"recording-start",code:1) }
print("READY"); fflush(stdout)
let input = RecorderDelegate()
DispatchQueue.global().async { _ = readLine(); input.set("stop") }
while input.get() != "stop" && delegate.get() == "ready" {
    try await Task.sleep(for:.milliseconds(100))
}
let requested = input.get() == "stop"
if delegate.get() == "ready" {
    try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
        stream.stopCapture { error in if let error { c.resume(throwing:error) } else { c.resume() } }
    }
}
for _ in 0..<100 { if delegate.get() == "finished" || delegate.get() == "failed" { break }; try await Task.sleep(for:.milliseconds(100)) }
guard delegate.get() == "finished" else { throw NSError(domain:"recording-finish",code:1) }
let asset = AVURLAsset(url:outputConfig.outputURL)
let duration = try await asset.load(.duration).seconds
guard duration > 0, try await asset.loadTracks(withMediaType:.video).count == 1, try await asset.loadTracks(withMediaType:.audio).isEmpty else { throw NSError(domain:"recording-validation",code:1) }
print("FINISHED duration=\(duration)"); fflush(stdout)
if !requested { exit(2) }
