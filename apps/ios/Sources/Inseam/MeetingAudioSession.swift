import AVFoundation

/// Configure the physical input once so channels keep their meaning for the whole meeting.
enum MeetingAudioSession {
    static func configure(orientation: AVAudioSession.StereoOrientation) throws -> MeetingAudioMetadata {
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(.record, mode: .default, options: [])
        try session.setPreferredSampleRate(48_000)
        try session.setActive(true, options: [])
        guard let input = session.availableInputs?.first(where: { $0.portType == .builtInMic }) else {
            throw MeetingRecordingError.noMicrophone
        }
        try session.setPreferredInput(input)
        let stereoSource = input.dataSources?.first { $0.supportedPolarPatterns?.contains(.stereo) == true }
        if let stereoSource {
            try stereoSource.setPreferredPolarPattern(.stereo)
            try input.setPreferredDataSource(stereoSource)
            try session.setPreferredInputOrientation(orientation)
            try session.setPreferredInputNumberOfChannels(2)
        } else {
            try session.setPreferredInputNumberOfChannels(1)
        }
        guard let active = session.currentRoute.inputs.first else {
            throw MeetingRecordingError.noMicrophone
        }
        guard active.portType == .builtInMic else { throw MeetingRecordingError.inputChanged }
        let channels = session.inputNumberOfChannels
        guard (1...2).contains(channels) else { throw MeetingRecordingError.invalidChannels }
        if channels == 2 {
            guard session.inputOrientation != .none else {
                throw MeetingRecordingError.invalidChannels
            }
            guard active.selectedDataSource?.selectedPolarPattern == .stereo else {
                throw MeetingRecordingError.invalidChannels
            }
        }
        return MeetingAudioMetadata(
            channels: UInt32(channels), sampleRate: session.sampleRate,
            inputUID: active.uid, inputName: active.portName,
            dataSource: active.selectedDataSource?.dataSourceName,
            polarPattern: active.selectedDataSource?.selectedPolarPattern?.rawValue,
            inputOrientation: channels == 2 ? session.inputOrientation.rawValue : 0
        )
    }

    static func settings(for audio: MeetingAudioMetadata) -> [String: Any] {
        assert((1...2).contains(audio.channels))
        assert(audio.sampleRate > 0)
        return [
            AVFormatIDKey: kAudioFormatAppleLossless,
            AVSampleRateKey: audio.sampleRate,
            AVNumberOfChannelsKey: audio.channels,
            AVEncoderBitDepthHintKey: 16
        ]
    }
}

enum MeetingRecordingError: LocalizedError {
    case noMicrophone
    case permissionDenied
    case inputChanged
    case invalidChannels
    case storageLow
    case couldNotStart
    case encodingFailed

    var errorDescription: String? {
        switch self {
        case .noMicrophone: return "No built-in microphone is available."
        case .permissionDenied: return "Allow microphone access in Settings to record a meeting."
        case .inputChanged: return "The built-in microphone could not be selected."
        case .invalidChannels: return "The microphone did not provide a supported mono or stereo input."
        case .storageLow: return "Free at least 3 GB on this device before recording a meeting."
        case .couldNotStart: return "Audio recording could not start. Another app or call may be using the microphone."
        case .encodingFailed: return "Audio recording failed. Any partial file remains in Call Recordings."
        }
    }
}
