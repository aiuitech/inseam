import Foundation

/// Actual capture geometry is retained for later analysis, without claiming speaker identities.
struct MeetingAudioMetadata: Codable, Equatable {
    var channels: UInt32
    var sampleRate: Double
    let inputUID: String
    let inputName: String
    let dataSource: String?
    let polarPattern: String?
    let inputOrientation: Int
}

struct MeetingMetadata: Codable {
    let id: UUID
    let name: String
    var audio: MeetingAudioMetadata
    var durationSeconds: Double?
    var stopReason: StopReason?

    enum StopReason: String, Codable {
        case user
        case durationLimit
        case interruption
        case routeChange
        case mediaReset
        case encodingFailure
    }
}
