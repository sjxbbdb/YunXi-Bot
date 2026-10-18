import AppKit
import CoreLocation

// One-shot acquisition only; precise coordinates never cross the native boundary.
// CoreLocation delivers callbacks on the run loop where this manager is created (main).
@MainActor final class NativeLocation: NSObject, @preconcurrency CLLocationManagerDelegate {
    private let manager = CLLocationManager()
    private var pending: CheckedContinuation<[String: Any], Error>?
    private var asking = false
    private var initialized = false
    private var timeout: Task<Void, Never>?

    override init() {
        super.init()
        manager.delegate = self
        manager.desiredAccuracy = kCLLocationAccuracyKilometer
    }
    static func status() async -> String {
        let service = NativeLocation()
        guard await service.waitForAuthorizationState() else { return "unavailable" }
        return service.currentStatus()
    }
    private func waitForAuthorizationState() async -> Bool {
        // A fresh manager reports notDetermined until locationd delivers its initial state.
        for _ in 0..<30 {
            if initialized { return true }
            do { try await Task.sleep(nanoseconds: 100_000_000) } catch { return false }
        }
        return initialized
    }
    private func currentStatus() -> String {
        guard CLLocationManager.locationServicesEnabled() else { return "services_disabled" }
        switch manager.authorizationStatus {
        case .authorizedAlways, .authorizedWhenInUse: return "full_access"
        case .notDetermined: return "not_determined"
        case .denied: return "denied"
        case .restricted: return "restricted"
        @unknown default: return "unavailable"
        }
    }
    static func coarse(_ location: CLLocation, now: Date = Date()) throws -> [String: Any] {
        guard CLLocationCoordinate2DIsValid(location.coordinate),
              location.horizontalAccuracy >= 0, location.horizontalAccuracy <= 20_000,
              now.timeIntervalSince(location.timestamp) >= -10,
              now.timeIntervalSince(location.timestamp) <= 300 else {
            throw Failure("location_unavailable", "系统位置过期或精度不足；未使用IP位置或旧地址替代")
        }
        return ["latitude": (location.coordinate.latitude * 10).rounded() / 10,
                "longitude": (location.coordinate.longitude * 10).rounded() / 10,
                "coordinate_decimals": 1, "source": "macOS CoreLocation",
                "located_at": stamp(location.timestamp), "approximate": true]
    }
    func run(authorize: Bool) async throws -> [String: Any] {
        guard await waitForAuthorizationState() else { throw Failure("location_unavailable", "系统定位权限状态尚未就绪") }
        let state = currentStatus()
        if authorize && state != "not_determined" { return ["location": state] }
        if !authorize && state != "full_access" {
            throw Failure("permission_required", "定位未就绪（\(state)）；请运行 mac authorize location 并在系统中允许。未查询天气。")
        }
        asking = authorize
        return try await withCheckedThrowingContinuation { continuation in
            pending = continuation
            timeout = Task { @MainActor [weak self] in
                do { try await Task.sleep(nanoseconds: authorize ? 100_000_000_000 : 18_000_000_000) }
                catch { return }
                self?.finish(.failure(Failure("location_unavailable", "系统定位或授权超时；未使用IP位置替代")))
            }
            if authorize {
                NSApplication.shared.activate(ignoringOtherApps: true)
                manager.requestWhenInUseAuthorization()
            } else { manager.requestLocation() }
        }
    }
    private func finish(_ result: Result<[String: Any], Error>) {
        guard let continuation = pending else { return }
        pending = nil
        timeout?.cancel()
        manager.stopUpdatingLocation()
        continuation.resume(with: result)
    }
    func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        initialized = true
        if asking && manager.authorizationStatus != .notDetermined {
            finish(.success(["location": currentStatus()]))
        } else if !asking && pending != nil && currentStatus() != "full_access" {
            finish(.failure(Failure("permission_required", "系统定位权限已撤销；未查询天气")))
        }
    }
    func locationManager(_ manager: CLLocationManager, didUpdateLocations locations: [CLLocation]) {
        do {
            guard let location = locations.max(by: { $0.timestamp < $1.timestamp }) else {
                throw Failure("location_unavailable", "系统未返回位置")
            }
            finish(.success(try Self.coarse(location)))
        } catch { finish(.failure(error)) }
    }
    func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {
        finish(.failure(Failure("location_unavailable", "系统暂时无法定位；请检查定位服务及无线网络，未使用IP位置替代")))
    }
    static func selfTest() throws {
        let now = Date()
        let result = try coarse(CLLocation(coordinate: CLLocationCoordinate2D(latitude: 31.23456, longitude: 121.48765), altitude: 0, horizontalAccuracy: 100, verticalAccuracy: -1, timestamp: now), now: now)
        guard result["latitude"] as? Double == 31.2, result["longitude"] as? Double == 121.5 else { throw Failure("test_failed", "定位没有粗化") }
        for (age, accuracy) in [(301.0, 100.0), (-11, 100), (0, -1), (0, 20_001)] {
            do {
                _ = try coarse(CLLocation(coordinate: CLLocationCoordinate2D(latitude: 31, longitude: 121), altitude: 0, horizontalAccuracy: accuracy, verticalAccuracy: -1, timestamp: now.addingTimeInterval(-age)), now: now)
                throw Failure("test_failed", "无效定位未拒绝")
            } catch let error as Failure where error.code == "location_unavailable" { }
        }
        print("location validation self-test: 5 passed")
    }
}
