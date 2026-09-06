import Foundation
import InseamKit

/// Call recordings as a bridged host (design/ios-app.md). iOS lets no
/// third-party app hear a phone call, so the recording comes from Apple's
/// own call recorder: the user records in the Phone app, the audio and
/// transcript land in Notes, and the user hands them to inseam (share
/// sheet → "inseam", or Notes' *Save Audio to Files* into this folder).
/// The folder is the app's Documents › Call Recordings, visible in the
/// Files app as "On My iPhone › inseam".
///
/// One recording is up to three sources sharing a stem:
/// `<stem>.m4a` (the audio), `<stem>.txt` (the transcript, from Apple's
/// or from on-device transcription here), and `<stem>.json` (the
/// sidecar: participant and call times, which become envelope
/// properties on the other two).
final class CallRecordingsHost: BridgedHostSource, @unchecked Sendable {
    static let kind = "call-recordings"
    static let filesPerEnumerationMax = 10_000
    static let audioExtensions: Set<String> = ["m4a", "mp3", "wav", "caf", "aac"]

    let folder: URL
    let description: BridgedHostDescription

    init(folder: URL) {
        self.folder = folder
        let device = ProcessInfo.processInfo.hostName
        description = BridgedHostDescription(
            kind: Self.kind,
            principal: "calls:\(device)",
            displayName: "Call recordings on \(device)"
        )
    }

    func enumerate(root: String) throws -> [BridgedSource] {
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let keys: [URLResourceKey] = [.fileSizeKey, .creationDateKey, .contentModificationDateKey, .isRegularFileKey]
        let entries = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: keys)
        var sources: [BridgedSource] = []
        for url in entries.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }).prefix(Self.filesPerEnumerationMax) {
            let values = try url.resourceValues(forKeys: Set(keys))
            guard values.isRegularFile == true else { continue }
            guard let contentType = Self.contentType(of: url) else { continue }
            let locator = url.lastPathComponent
            guard locator.hasPrefix(root) else { continue }
            let sidecar = Sidecar.read(beside: url)
            sources.append(BridgedSource(
                locator: locator,
                sourceType: contentType.hasPrefix("audio/") ? "call-recording" : "call-transcript",
                contentType: contentType,
                bytes: UInt64(values.fileSize ?? 0),
                created: (sidecar?.callStarted ?? values.creationDate).map { Int64($0.timeIntervalSince1970) },
                modified: values.contentModificationDate.map { Int64($0.timeIntervalSince1970) },
                title: sidecar?.title ?? url.deletingPathExtension().lastPathComponent,
                properties: sidecar?.properties ?? []
            ))
        }
        return sources
    }

    func readBytes(locator: String) throws -> Data {
        // Locators are bare file names; a path separator is not one.
        guard !locator.contains("/"), !locator.hasPrefix(".") else {
            throw RecordingsError.badLocator(locator)
        }
        return try Data(contentsOf: folder.appendingPathComponent(locator))
    }

    /// Copy a recording (or transcript) the user handed us into the folder
    /// under a stem naming the call, and write the sidecar. Returns the
    /// imported file's URL.
    func importRecording(from source: URL, participant: String, call: ObservedCall?) throws -> URL {
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let scoped = source.startAccessingSecurityScopedResource()
        defer { if scoped { source.stopAccessingSecurityScopedResource() } }
        let started = call?.started ?? Date()
        let stem = Self.stem(participant: participant, started: started)
        let destination = folder.appendingPathComponent(stem).appendingPathExtension(source.pathExtension)
        if FileManager.default.fileExists(atPath: destination.path) {
            try FileManager.default.removeItem(at: destination)
        }
        try FileManager.default.copyItem(at: source, to: destination)
        let sidecar = Sidecar(participant: participant, callStarted: started, callEnded: call?.ended)
        try sidecar.write(beside: destination)
        return destination
    }

    /// `2026-09-06T14-05_+15550100`: sortable, filesystem-safe.
    static func stem(participant: String, started: Date) -> String {
        let safe = participant.map { $0.isLetter || $0.isNumber || $0 == "+" ? $0 : "-" }
        return "\(stemFormatter.string(from: started))_\(String(safe))"
    }

    private static let stemFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyy-MM-dd'T'HH-mm"
        return formatter
    }()

    private static func contentType(of url: URL) -> String? {
        let ext = url.pathExtension.lowercased()
        if audioExtensions.contains(ext) {
            return ext == "m4a" ? "audio/mp4" : "audio/\(ext)"
        }
        if ext == "txt" {
            return "text/plain"
        }
        return nil
    }
}

/// The sidecar beside a recording: who the call was with and when. The
/// participant is a claimed property — it is what the owner said, never
/// verified — which is exactly the envelope's trust vocabulary.
struct Sidecar: Codable {
    var participant: String
    var callStarted: Date
    var callEnded: Date?

    var title: String {
        "Call with \(participant), \(Self.titleFormatter.string(from: callStarted))"
    }

    var properties: [BridgedProperty] {
        [BridgedProperty(key: "participant", value: participant)]
    }

    static func read(beside url: URL) -> Sidecar? {
        let path = url.deletingPathExtension().appendingPathExtension("json")
        guard let data = try? Data(contentsOf: path) else { return nil }
        return try? decoder.decode(Sidecar.self, from: data)
    }

    func write(beside url: URL) throws {
        let path = url.deletingPathExtension().appendingPathExtension("json")
        try Self.encoder.encode(self).write(to: path, options: .atomic)
    }

    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.sortedKeys, .prettyPrinted]
        return encoder
    }()

    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }()

    private static let titleFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateStyle = .medium
        formatter.timeStyle = .short
        return formatter
    }()
}

enum RecordingsError: LocalizedError {
    case badLocator(String)

    var errorDescription: String? {
        switch self {
        case .badLocator(let locator):
            return "\(locator) is not a recording in the Call Recordings folder"
        }
    }
}
