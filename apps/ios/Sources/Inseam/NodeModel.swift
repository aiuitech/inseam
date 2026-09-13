import Foundation
import InseamKit
import Observation

/// Owns the node handle and marshals blocking FFI calls off the main
/// actor. The iOS node is a leaf steward (design/ios-app.md): it opens
/// with the device's own embedder, bridges the hosts only this app can
/// reach, indexes them while the app is in front, and answers queries.
@MainActor
@Observable
final class NodeModel {
    private(set) var status = "opening node…"
    private(set) var results: [QueryResult] = []
    private(set) var busy = false
    private(set) var nodeOpen = false
    private(set) var hosts: [HostView] = []
    private(set) var indexProgress: IndexProgress?
    private(set) var indexPaused = false
    private(set) var indexStopping = false
    /// Failed and pending composition entries, one line each.
    private(set) var parkedWarnings: [String] = []
    /// A recording the user is attaching to a call (the attach sheet).
    var pendingAttachment: PendingAttachment?
    /// The owner's always-on node, when its URL and owner token are saved:
    /// call capture is that node's to place (design/call-capture.md).
    private(set) var hostedNode: HostedNodeClient?

    let coreVersion = CoreNode.coreVersion()
    let dataDir: URL
    let recordings: CallRecordingsHost
    let callObserver = CallObserver()

    private var node: CoreNode?
    private var recordingsHostId: String?
    private var indexController: IndexController?

    var indexingActive: Bool { indexController != nil }
    var compositionURL: URL { dataDir.appendingPathComponent("composition.toml") }

    init() {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        dataDir = support.appendingPathComponent("inseam")
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        recordings = CallRecordingsHost(folder: documents.appendingPathComponent("Call Recordings"))
        reloadHostedNode()
    }

    /// Re-read the hosted node's URL and token after settings change.
    func reloadHostedNode() {
        hostedNode = HostedNodeClient.savedConfiguration().map { HostedNodeClient(configuration: $0) }
    }

    /// Open the node with the on-device embedder when the device has one,
    /// else plain (vectors then need an endpoint key). Then bridge the
    /// hosts in; a reopen re-registers them because the registry is per
    /// handle.
    func openNode() {
        let old = node
        node = nil
        nodeOpen = false
        hosts = []
        status = "opening node…"
        let dataDir = dataDir
        let recordings = recordings
        run("open node") { [weak self] in
            old?.close()
            try FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)
            try SecretStore.exportIntoEnvironment()
            var node = try Self.makeNode(dataDir: dataDir)
            var health = try node.health()
            let missingNames = health.flatMap(\.missingSecrets).map(\.env)
            if try SecretStore.exportDeclaredIntoEnvironment(names: missingNames) {
                node.close()
                node = try Self.makeNode(dataDir: dataDir)
                health = try node.health()
            }
            let recordingsView = try node.registerHost(recordings.description, source: recordings)
            if let photos = PhotosHost.ifAuthorized() {
                _ = try node.registerHost(photos.description, source: photos)
            }
            let hosts = try node.hosts()
            return {
                self?.node = node
                self?.nodeOpen = true
                self?.recordingsHostId = recordingsView.id
                self?.hosts = hosts
                self?.parkedWarnings = Self.parkedWarnings(in: health)
                self?.status = "node open · core \(self?.coreVersion ?? "")"
            }
        }
    }

    func query(_ text: String) {
        guard let node, !text.isEmpty else { return }
        run("query") { [weak self] in
            let response = try node.query(text)
            return {
                self?.results = response.results
                self?.status = response.results.isEmpty
                    ? "no results — index something first"
                    : "\(response.results.count) result(s)"
            }
        }
    }

    /// Ask for full photo access if needed, then bridge and sweep the
    /// library. A large library is many sources; the sweep is bounded per
    /// run and resumable, so this can be run again to continue.
    func indexPhotos() {
        guard let node else { return }
        startIndex(label: "photos", node: node) { controller in
            let photos = try PhotosHost.requestingAccess()
            let view: HostView
            if let existing = try node.hosts().first(where: { $0.kind == PhotosHost.kind }) {
                view = existing
            } else {
                view = try node.registerHost(photos.description, source: photos)
            }
            return try node.indexHost(view.id, controller: controller)
        }
    }

    func indexCallRecordings() {
        guard let node, let hostId = recordingsHostId else { return }
        startIndex(label: "call recordings", node: node) { controller in
            try node.indexHost(hostId, controller: controller)
        }
    }

    func pauseIndexing() {
        indexController?.pause()
        indexPaused = true
    }

    func resumeIndexing() {
        indexController?.resume()
        indexPaused = false
    }

    func stopIndexing() {
        indexController?.stop()
        indexPaused = false
        indexStopping = true
    }

    func dismissIndexProgress() {
        guard !indexingActive else { return }
        indexProgress = nil
    }

    /// Start attaching a recording: from a file the share sheet handed us
    /// (`fileURL`), or from the picker when nil. The sheet collects the
    /// participant and the call, then `finishAttach` imports and indexes.
    func beginAttach(fileURL: URL?) {
        pendingAttachment = PendingAttachment(fileURL: fileURL, call: callObserver.recentCalls.first)
    }

    func finishAttach(_ attachment: PendingAttachment, participant: String) {
        guard let fileURL = attachment.fileURL else { return }
        let recordings = recordings
        let call = attachment.call
        pendingAttachment = nil
        run("attach recording") { [weak self] in
            let imported = try recordings.importRecording(from: fileURL, participant: participant, call: call)
            // Transcribe on device so the words are searchable; the
            // transcript is its own text source beside the audio.
            try Transcription.transcribe(imported, into: recordings)
            return {
                self?.status = "attached \(imported.lastPathComponent)"
                Task { @MainActor [weak self] in
                    self?.indexCallRecordings()
                }
            }
        }
    }

    /// Capture has already saved the original. Transcription failure leaves it available in Files.
    func processMeeting(at url: URL) {
        guard !busy else {
            status = "recording saved; index it when the current operation finishes"
            return
        }
        let recordings = recordings
        run("transcribe meeting") { [weak self] in
            try Transcription.transcribe(url, into: recordings)
            return {
                self?.status = "saved \(url.lastPathComponent)"
                Task { @MainActor [weak self] in self?.indexCallRecordings() }
            }
        }
    }

    private static func describe(_ report: IndexReport, of what: String) -> String {
        let verb = report.stopped ? "stopped" : "indexed"
        return "\(verb) \(what): \(report.indexed) indexed, \(report.unchanged) unchanged, \(report.fragments) fragments"
    }

    nonisolated private static func makeNode(dataDir: URL) throws -> CoreNode {
        if let embedder = AppleEmbedder.ifAvailable() {
            return try CoreNode(dataDir: dataDir, embedder: embedder)
        }
        return try CoreNode(dataDir: dataDir)
    }

    private func startIndex(
        label: String,
        node: CoreNode,
        work: @escaping @Sendable (IndexController) throws -> IndexReport
    ) {
        let controller = IndexController { [weak self] progress in
            Task { @MainActor in self?.indexProgress = progress }
        }
        indexController = controller
        indexPaused = false
        indexStopping = false
        busy = true
        Task.detached(priority: .userInitiated) {
            do {
                let report = try work(controller)
                let hosts = (try? node.hosts()) ?? []
                await MainActor.run {
                    self.hosts = hosts
                    self.status = Self.describe(report, of: label)
                    self.finishIndexState()
                }
            } catch {
                await MainActor.run {
                    self.status = "index \(label) failed: \(error.localizedDescription)"
                    self.finishIndexState()
                }
            }
        }
    }

    private func finishIndexState() {
        indexController = nil
        indexPaused = false
        indexStopping = false
        busy = false
    }

    private static func parkedWarnings(in health: [FiberHealth]) -> [String] {
        var lines: [String] = []
        for entry in health where entry.state == "failed" {
            lines.append("\(entry.id): \(entry.error ?? "failed")")
        }
        let pending = health.filter { $0.state == "pending" }.map(\.id)
        if !pending.isEmpty {
            lines.append("parked with it: \(pending.joined(separator: ", "))")
        }
        return lines
    }

    /// Run blocking core work off the main actor, then apply its
    /// main-actor completion. Errors land in `status`.
    private func run(_ label: String, _ work: @escaping @Sendable () throws -> @MainActor () -> Void) {
        busy = true
        Task.detached(priority: .userInitiated) {
            do {
                let apply = try work()
                await MainActor.run {
                    apply()
                    self.busy = false
                }
            } catch {
                await MainActor.run {
                    self.status = "\(label) failed: \(error.localizedDescription)"
                    self.busy = false
                }
            }
        }
    }
}

/// What the attach sheet works on: the file (nil until picked) and the
/// call it most likely belongs to.
struct PendingAttachment: Identifiable {
    let id = UUID()
    var fileURL: URL?
    var call: ObservedCall?
}
