import AVFoundation
import Foundation
import Speech

/// On-device transcription of an imported recording with the iOS 26
/// Speech framework (`SpeechAnalyzer` + `SpeechTranscriber`): no server,
/// no duration cap, the model downloaded once into the system's asset
/// store. The transcript is written beside the audio as `<stem>.txt`, so
/// the call recordings host serves it as a text source the sweep can
/// chunk, embed, and search. No speaker diarization is available from
/// the framework, so the transcript is one voice.
enum Transcription {
    /// Longest recording transcribed in one go.
    static let durationSecondsMax: Double = 4 * 60 * 60

    static func transcribe(_ audioURL: URL, into host: CallRecordingsHost) throws {
        guard CallRecordingsHost.audioExtensions.contains(audioURL.pathExtension.lowercased()) else {
            // A transcript the user handed us directly needs no transcription.
            return
        }
        let transcriptURL = audioURL.deletingPathExtension().appendingPathExtension("txt")
        guard !FileManager.default.fileExists(atPath: transcriptURL.path) else { return }
        let semaphore = DispatchSemaphore(value: 0)
        var outcome: Result<String, Error> = .failure(TranscriptionError.notStarted)
        Task {
            do {
                outcome = .success(try await transcript(of: audioURL))
            } catch {
                outcome = .failure(error)
            }
            semaphore.signal()
        }
        semaphore.wait()
        let text = try outcome.get()
        try text.write(to: transcriptURL, atomically: true, encoding: .utf8)
    }

    private static func transcript(of audioURL: URL) async throws -> String {
        let file = try AVAudioFile(forReading: audioURL)
        let seconds = Double(file.length) / file.fileFormat.sampleRate
        guard seconds <= durationSecondsMax else { throw TranscriptionError.tooLong(seconds) }
        let locale = Locale.current
        let transcriber = SpeechTranscriber(locale: locale, preset: .transcription)
        if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
            try await request.downloadAndInstall()
        }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        var lines: [String] = []
        let collector = Task {
            for try await result in transcriber.results where result.isFinal {
                lines.append(String(result.text.characters))
            }
        }
        _ = try await analyzer.analyzeSequence(from: file)
        try await analyzer.finalizeAndFinishThroughEndOfInput()
        try await collector.value
        return lines.joined(separator: "\n")
    }
}

enum TranscriptionError: LocalizedError {
    case notStarted
    case tooLong(Double)

    var errorDescription: String? {
        switch self {
        case .notStarted:
            return "transcription did not start"
        case .tooLong(let seconds):
            return "recording is \(Int(seconds / 60)) minutes; the ceiling is \(Int(Transcription.durationSecondsMax / 60))"
        }
    }
}
