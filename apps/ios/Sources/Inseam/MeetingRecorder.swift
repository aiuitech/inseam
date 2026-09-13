import AVFoundation
import Foundation
import Observation

@MainActor
@Observable
final class MeetingRecorder: NSObject, AVAudioRecorderDelegate {
    static let durationSecondsMax: Double = 4 * 60 * 60
    static let storageBytesMinimum: Int64 = 3_000_000_000

    enum State {
        case ready
        case requestingPermission
        case recording(Date, MeetingAudioMetadata)
        case saved(URL, MeetingMetadata.StopReason)
        case failed(String)
    }

    private(set) var state: State = .ready
    @ObservationIgnored private var recorder: AVAudioRecorder?
    @ObservationIgnored private var sidecar: Sidecar?
    @ObservationIgnored private var stopReason: MeetingMetadata.StopReason?

    var active: Bool {
        switch state {
        case .requestingPermission, .recording: return true
        case .ready, .saved, .failed: return false
        }
    }

    var savedURL: URL? {
        guard case .saved(let url, _) = state else { return nil }
        return url
    }

    override init() {
        super.init()
        let center = NotificationCenter.default
        center.addObserver(self, selector: #selector(interrupted), name: AVAudioSession.interruptionNotification, object: nil)
        center.addObserver(self, selector: #selector(routeChanged), name: AVAudioSession.routeChangeNotification, object: nil)
        center.addObserver(self, selector: #selector(mediaReset), name: AVAudioSession.mediaServicesWereResetNotification, object: nil)
    }

    func start(name: String, folder: URL, orientation: AVAudioSession.StereoOrientation) async {
        guard !active else { return }
        state = .requestingPermission
        guard await AVAudioApplication.requestRecordPermission() else {
            state = .failed(MeetingRecordingError.permissionDenied.localizedDescription)
            return
        }
        do {
            try prepare(name: name, folder: folder, orientation: orientation)
        } catch {
            recorder?.delegate = nil
            recorder?.stop()
            recorder = nil
            sidecar = nil
            releaseSession()
            state = .failed(error.localizedDescription)
        }
    }

    private func prepare(name: String, folder: URL, orientation: AVAudioSession.StereoOrientation) throws {
        assert(recorder == nil)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let capacity = try folder.resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey])
        guard let available = capacity.volumeAvailableCapacityForImportantUsage else {
            throw MeetingRecordingError.storageLow
        }
        guard available >= Self.storageBytesMinimum else { throw MeetingRecordingError.storageLow }
        let audio = try MeetingAudioSession.configure(orientation: orientation)
        let id = UUID()
        let url = folder.appendingPathComponent("meeting-\(id.uuidString).caf")
        let started = Date()
        let trimmedName = String(name.trimmingCharacters(in: .whitespacesAndNewlines).prefix(200))
        let meeting = MeetingMetadata(id: id, name: trimmedName.isEmpty ? "Meeting" : trimmedName, audio: audio)
        let metadata = Sidecar(participant: "", callStarted: started, meeting: meeting)
        try metadata.write(beside: url)
        let recording = try AVAudioRecorder(url: url, settings: MeetingAudioSession.settings(for: audio))
        guard recording.prepareToRecord() else { throw MeetingRecordingError.couldNotStart }
        guard recording.format.channelCount == audio.channels else {
            throw MeetingRecordingError.invalidChannels
        }
        recording.delegate = self
        sidecar = metadata
        stopReason = nil
        recorder = recording
        guard recording.record(forDuration: Self.durationSecondsMax) else {
            throw MeetingRecordingError.couldNotStart
        }
        state = .recording(started, audio)
    }

    func stop() {
        finish(reason: .user)
    }

    private func finish(reason: MeetingMetadata.StopReason) {
        guard case .recording = state else { return }
        guard stopReason == nil else { return }
        stopReason = reason
        recorder?.stop()
        // Finalize synchronously: stop closes the file, and a late delegate callback is ignored.
        complete(successfully: true)
    }

    private func complete(successfully: Bool) {
        guard let recorder else { return }
        self.recorder = nil
        recorder.delegate = nil
        recorder.stop()
        defer { releaseSession() }
        guard var metadata = sidecar else {
            assertionFailure("A recorder always has prewritten metadata.")
            return
        }
        do {
            let file = try AVAudioFile(forReading: recorder.url)
            let format = file.fileFormat
            guard format.channelCount == metadata.meeting?.audio.channels else {
                throw MeetingRecordingError.invalidChannels
            }
            assert(format.sampleRate > 0)
            metadata.meeting?.audio.sampleRate = format.sampleRate
            metadata.meeting?.durationSeconds = Double(file.length) / format.sampleRate
            metadata.meeting?.stopReason = successfully ? (stopReason ?? .durationLimit) : .encodingFailure
            metadata.callEnded = Date()
            try metadata.write(beside: recorder.url)
            sidecar = nil
            if successfully {
                state = .saved(recorder.url, stopReason ?? .durationLimit)
            } else {
                state = .failed(MeetingRecordingError.encodingFailed.localizedDescription)
            }
        } catch {
            state = .failed("The recording remains at \(recorder.url.lastPathComponent), but finalizing it failed: \(error.localizedDescription)")
        }
    }

    private func releaseSession() {
        do {
            try AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
        } catch {
            // The file is already closed. An interrupted session can already be inactive.
        }
    }

    @objc nonisolated private func interrupted(_ notification: Notification) {
        guard let value = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt else { return }
        guard value == AVAudioSession.InterruptionType.began.rawValue else { return }
        Task { @MainActor [weak self] in self?.finish(reason: .interruption) }
    }

    @objc nonisolated private func routeChanged(_ notification: Notification) {
        guard let value = notification.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt else { return }
        // Configuration generates these notifications before capture starts.
        guard value != AVAudioSession.RouteChangeReason.categoryChange.rawValue else { return }
        Task { @MainActor [weak self] in self?.finishIfInputChanged() }
    }

    private func finishIfInputChanged() {
        guard case .recording(_, let audio) = state else { return }
        let session = AVAudioSession.sharedInstance()
        let input = session.currentRoute.inputs.first
        if input?.uid != audio.inputUID {
            finish(reason: .routeChange)
        } else if session.inputNumberOfChannels != Int(audio.channels) {
            finish(reason: .routeChange)
        } else if input?.selectedDataSource?.dataSourceName != audio.dataSource {
            finish(reason: .routeChange)
        } else if input?.selectedDataSource?.selectedPolarPattern?.rawValue != audio.polarPattern {
            finish(reason: .routeChange)
        } else if session.sampleRate != audio.sampleRate {
            finish(reason: .routeChange)
        } else if audio.channels == 2 {
            if session.inputOrientation.rawValue != audio.inputOrientation {
                finish(reason: .routeChange)
            }
        }
    }

    @objc nonisolated private func mediaReset(_ notification: Notification) {
        Task { @MainActor [weak self] in self?.finish(reason: .mediaReset) }
    }

    nonisolated func audioRecorderDidFinishRecording(_ recorder: AVAudioRecorder, successfully flag: Bool) {
        let url = recorder.url
        Task { @MainActor [weak self] in
            guard self?.recorder?.url == url else { return }
            self?.complete(successfully: flag)
        }
    }

    nonisolated func audioRecorderEncodeErrorDidOccur(_ recorder: AVAudioRecorder, error: Error?) {
        let url = recorder.url
        Task { @MainActor [weak self] in
            guard self?.recorder?.url == url else { return }
            self?.complete(successfully: false)
        }
    }
}
