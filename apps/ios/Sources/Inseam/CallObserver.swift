import CallKit
import Foundation
import Observation
import UserNotifications

/// What CallKit lets any app see about the phone's calls: that one
/// started, connected, and ended — never the number, never the audio
/// (design/ios-app.md). That is enough to know *when* a call happened,
/// so that when the user attaches a recording the call's times are
/// already filled in, and to nudge the user once a call ends.
///
/// Observed only while the app is running; iOS does not wake an app for
/// calls. Bounded: the last `callsMax` calls are kept.
@MainActor
@Observable
final class CallObserver: NSObject, CXCallObserverDelegate {
    static let callsMax = 32

    private(set) var recentCalls: [ObservedCall] = []
    private let observer = CXCallObserver()
    private var inFlight: [UUID: Date] = [:]

    override init() {
        super.init()
        observer.setDelegate(self, queue: .main)
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    nonisolated func callObserver(_ callObserver: CXCallObserver, callChanged call: CXCall) {
        let uuid = call.uuid
        let connected = call.hasConnected
        let ended = call.hasEnded
        let outgoing = call.isOutgoing
        Task { @MainActor in
            self.record(uuid: uuid, connected: connected, ended: ended, outgoing: outgoing)
        }
    }

    private func record(uuid: UUID, connected: Bool, ended: Bool, outgoing: Bool) {
        if connected, !ended, inFlight[uuid] == nil {
            inFlight[uuid] = Date()
            return
        }
        guard ended, let started = inFlight.removeValue(forKey: uuid) else { return }
        let call = ObservedCall(id: uuid, started: started, ended: Date(), outgoing: outgoing)
        recentCalls.insert(call, at: 0)
        if recentCalls.count > Self.callsMax {
            recentCalls.removeLast(recentCalls.count - Self.callsMax)
        }
        assert(recentCalls.count <= Self.callsMax)
        Self.nudge(after: call)
    }

    /// A local notification once a call ends: iOS shows no third-party UI
    /// on the call screen, so this is the "Record?" moment we can offer —
    /// after the fact, pointing at Apple's recorder and our attach flow.
    private static func nudge(after call: ObservedCall) {
        let content = UNMutableNotificationContent()
        content.title = "Call ended"
        content.body = "Recorded it in Phone? Share the recording from Notes to inseam to index it."
        let request = UNNotificationRequest(
            identifier: "call-\(call.id.uuidString)",
            content: content,
            trigger: nil
        )
        UNUserNotificationCenter.current().add(request)
    }
}

/// One call as observed: times and direction. No number — CallKit does
/// not expose it for calls the app did not place.
struct ObservedCall: Identifiable, Sendable {
    let id: UUID
    let started: Date
    let ended: Date
    let outgoing: Bool
}
