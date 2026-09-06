import AppKit
import InseamKit
import SwiftUI

/// Owns the node handle and marshals blocking FFI calls off the main thread.
@MainActor
final class AppModel: ObservableObject {
    @Published var status = "opening node…"
    @Published var results: [QueryResult] = []
    @Published var busy = false
    @Published private(set) var nodeOpen = false
    /// Human-readable lines describing parked composition entries — empty
    /// when the node is fully settled.
    @Published private(set) var parkedWarnings: [String] = []
    /// Secrets parked entries declared as needed, deduplicated by variable
    /// name — when nonempty the UI asks for these instead of showing the
    /// raw parked warnings.
    @Published private(set) var neededSecrets: [SecretNeed] = []
    /// Ids of every non-active composition entry.
    @Published private(set) var parkedEntries: [String] = []
    /// Which Settings tab is showing; the "Add API Key…" button steers it.
    @Published var settingsTab: SettingsTab = .configuration
    /// Variable name the Secrets tab pre-fills, set when the main window
    /// sends the user there to satisfy a declared need.
    @Published var suggestedSecretName: String?
    /// The OAuth grants the node holds, refreshed after every open and
    /// every authorization or revocation.
    @Published private(set) var grants: [GrantView] = []
    /// The hosts the node stewards — the Google services appear here the
    /// moment their grant is authorized.
    @Published private(set) var hosts: [HostView] = []
    /// The grant a browser sign-in is in flight for, if any.
    @Published private(set) var authorizingGrant: String?
    /// The last outcome of a connect/disconnect, for the Settings page.
    @Published var connectionMessage = ""
    /// Every composition entry as the kernel runs it. The Plugins tab splits
    /// loaded artifacts from linked entries for display.
    @Published private(set) var pluginEntries: [PluginView] = []
    /// Plugin reads and installs block in the FFI and share this guard so the
    /// Settings tab cannot submit overlapping work.
    @Published private(set) var pluginBusy = false
    /// The last plugin list or install outcome shown in Settings.
    @Published private(set) var pluginMessage = ""
    /// The latest snapshot from the current or most recent index run.
    @Published private(set) var indexProgress: IndexProgress?
    @Published private(set) var indexPaused = false
    @Published private(set) var indexStopping = false

    let coreVersion = CoreNode.coreVersion()
    let dataDir: URL
    private var node: CoreNode?
    private var indexController: IndexController?

    var indexingActive: Bool { indexController != nil }

    init() {
        let base = FileManager.default.urls(
            for: .applicationSupportDirectory, in: .userDomainMask
        )[0]
        dataDir = base.appendingPathComponent("inseam")
    }

    var compositionURL: URL {
        dataDir.appendingPathComponent("composition.toml")
    }

    /// Open the node, closing any previous one first — Settings saves call
    /// this again, so a reopen must release the data dir before the next
    /// boot. Keychain secrets export into the environment before open so
    /// `key_env`-style config fields resolve.
    func openNode() {
        let old = node
        node = nil
        nodeOpen = false
        parkedWarnings = []
        neededSecrets = []
        parkedEntries = []
        pluginEntries = []
        results = []
        status = "opening node…"
        let dataDir = dataDir
        run("node open") { [weak self] in
            old?.close()
            let (node, health) = try Self.openNodeUsingKeychain(dataDir: dataDir)
            let parked = health.filter { $0.state != "active" }
            var seen = Set<String>()
            let needs = parked.flatMap(\.missingSecrets)
                .filter { seen.insert($0.env).inserted }
            let warnings = Self.parkedWarnings(in: health)
            let grants = (try? node.grants()) ?? []
            let hosts = (try? node.hosts()) ?? []
            let plugins = (try? node.plugins()) ?? []
            return {
                self?.node = node
                self?.nodeOpen = true
                self?.parkedEntries = parked.map(\.id)
                self?.neededSecrets = needs
                self?.parkedWarnings = warnings
                self?.grants = grants
                self?.hosts = hosts
                self?.pluginEntries = plugins
                if !needs.isEmpty {
                    self?.status = "node open — an API key is needed"
                } else if !warnings.isEmpty {
                    self?.status = "node open, but some entries are parked:"
                } else {
                    self?.status = "node open · data dir \(dataDir.path)"
                }
            }
        }
    }

    /// Re-read grants and hosts — after a sign-in or a revocation, nothing
    /// else changes, so the node stays open.
    func refreshConnections() {
        guard let node else { return }
        run("refresh connections") { [weak self] in
            let grants = try node.grants()
            let hosts = try node.hosts()
            return {
                self?.grants = grants
                self?.hosts = hosts
            }
        }
    }

    /// Re-read the running composition for the Plugins tab.
    func refreshPlugins() {
        guard !pluginBusy else { return }
        guard let node else {
            pluginEntries = []
            pluginMessage = "The node is not open."
            return
        }
        pluginMessage = "Loading plugins…"
        runPlugin { [weak self] in
            let entries = try node.plugins()
            return {
                self?.pluginEntries = entries
                self?.pluginMessage = "\(entries.count) entries loaded."
            }
        }
    }

    /// Install and reconcile a plugin without reopening the node. Completion
    /// receives the core's verbatim error, or nil after the refreshed list is
    /// visible.
    func installPlugin(
        id: String,
        directory: URL,
        completion: @escaping @MainActor (String?) -> Void
    ) {
        guard !pluginBusy else {
            completion("another plugin operation is already running")
            return
        }
        guard let node else {
            completion("node is not open")
            return
        }
        pluginMessage = "Installing \(id)…"
        runPlugin(
            { [weak self] in
                let installed = try node.installPlugin(id: id, directory: directory)
                let entries = try node.plugins()
                return {
                    self?.pluginEntries = entries
                    self?.pluginMessage = "\(installed.id): \(installed.stateLabel)"
                    completion(nil)
                }
            },
            onError: { [weak self] message in
                self?.pluginMessage = message
                completion(message)
            }
        )
    }

    private func runPlugin(
        _ work: @escaping () throws -> @MainActor () -> Void,
        onError: @escaping @MainActor (String) -> Void = { _ in }
    ) {
        pluginBusy = true
        Task.detached(priority: .userInitiated) {
            do {
                let apply = try work()
                await MainActor.run {
                    apply()
                    self.pluginBusy = false
                }
            } catch {
                let message = error.localizedDescription
                await MainActor.run {
                    self.pluginMessage = message
                    self.pluginBusy = false
                    onError(message)
                }
            }
        }
    }

    /// Authorize a grant: begin on the node (it listens on the loopback
    /// port), send the owner's browser to the provider, and wait off the
    /// main thread for the browser to come back.
    func authorize(grant: String) {
        guard let node else {
            connectionMessage = "node is not open"
            return
        }
        authorizingGrant = grant
        connectionMessage = "Waiting for the browser…"
        run("authorize \(grant)") { [weak self] in
            let started = try node.authorizeBegin(grant: grant)
            guard let url = URL(string: started.url) else {
                throw CoreError(message: "the provider URL is not valid: \(started.url)")
            }
            DispatchQueue.main.async { NSWorkspace.shared.open(url) }
            let view: GrantView
            do {
                view = try node.authorizeAwait(state: started.state)
            } catch {
                DispatchQueue.main.async {
                    self?.authorizingGrant = nil
                    self?.connectionMessage = error.localizedDescription
                }
                throw error
            }
            let grants = try node.grants()
            let hosts = try node.hosts()
            return {
                self?.authorizingGrant = nil
                self?.grants = grants
                self?.hosts = hosts
                let account = view.state.account.map { " as \($0)" } ?? ""
                self?.connectionMessage = "Connected\(account)."
            }
        }
    }

    /// Forget a grant's tokens; its hosts withdraw at once.
    func revoke(grant: String) {
        guard let node else {
            connectionMessage = "node is not open"
            return
        }
        run("disconnect \(grant)") { [weak self] in
            _ = try node.revokeGrant(grant)
            let grants = try node.grants()
            let hosts = try node.hosts()
            return {
                self?.grants = grants
                self?.hosts = hosts
                self?.connectionMessage = "Disconnected."
            }
        }
    }

    /// Open once with Inseam-owned secrets, then silently try exact Keychain
    /// service names for declared missing variables before the UI can ask.
    private static func openNodeUsingKeychain(
        dataDir: URL
    ) throws -> (CoreNode, [FiberHealth]) {
        try SecretStore.exportIntoEnvironment()
        let node = try CoreNode(dataDir: dataDir)
        let health = try node.health()
        let missingNames = health.flatMap(\.missingSecrets).map(\.env)
        guard try SecretStore.exportDeclaredIntoEnvironment(names: missingNames) else {
            return (node, health)
        }
        node.close()
        let reopenedNode = try CoreNode(dataDir: dataDir)
        return (reopenedNode, try reopenedNode.health())
    }

    /// Failed entries with their errors, then one line naming everything
    /// waiting on them.
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

    func query(_ text: String) {
        guard let node, !text.isEmpty else { return }
        run("query") { [weak self] in
            let response = try node.query(text)
            return {
                self?.results = response.results
                self?.status = response.results.isEmpty
                    ? "no results — index a folder first"
                    : "\(response.results.count) result(s)"
            }
        }
    }

    func indexFolder() {
        guard let node else {
            status = "node is not open — check Settings (⌘,)"
            return
        }
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.message = "Choose a folder to index"
        guard panel.runModal() == .OK, let dir = panel.url else { return }
        startIndex(label: dir.lastPathComponent) { controller in
            try node.indexDirectory(dir, controller: controller)
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

    private func startIndex(
        label: String,
        work: @escaping (IndexController) throws -> IndexReport
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
                await MainActor.run { self.finishIndex(label: label, report: report) }
            } catch {
                await MainActor.run {
                    self.status = "index \(label) failed: \(error.localizedDescription)"
                    self.finishIndexState()
                }
            }
        }
    }

    private func finishIndex(label: String, report: IndexReport) {
        let verb = report.stopped ? "stopped" : "indexed"
        status = "\(verb) \(label): \(report.indexed) indexed, "
            + "\(report.unchanged) unchanged, \(report.fragments) fragments"
        finishIndexState()
    }

    private func finishIndexState() {
        indexController = nil
        indexPaused = false
        indexStopping = false
        busy = false
    }

    /// Run blocking core work off the main actor, then apply its main-actor
    /// completion. Errors land in `status`.
    private func run(
        _ label: String,
        _ work: @escaping () throws -> @MainActor () -> Void
    ) {
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

/// The empty-state prompt for declared-but-missing secrets: each need's
/// purpose in plain language, one button to the pre-filled Secrets tab,
/// and a quiet line naming what stays paused meanwhile.
struct SecretPromptView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        VStack(spacing: 10) {
            ForEach(model.neededSecrets) { need in
                VStack(spacing: 4) {
                    Text(need.env)
                        .font(.callout.monospaced().bold())
                    Text(need.purpose)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .frame(maxWidth: 440)
            }
            Button("Add API Key…") {
                model.suggestedSecretName = model.neededSecrets.first?.env
                model.settingsTab = .secrets
                openSettings()
            }
            if !model.parkedEntries.isEmpty {
                Text("Paused meanwhile: \(model.parkedEntries.joined(separator: ", "))")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .center)
        .padding(.top, 6)
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @State private var queryText = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Inseam").font(.title2).bold()
                Text("core \(model.coreVersion)")
                    .font(.caption).foregroundStyle(.secondary)
                Spacer()
                if model.busy { ProgressView().controlSize(.small) }
                Button("Index Folder…") { model.indexFolder() }
                    .disabled(model.busy)
                SettingsLink {
                    Image(systemName: "gearshape")
                }
                .help("Settings (⌘,): composition and secrets")
            }

            TextField("Search your data…", text: $queryText)
                .textFieldStyle(.roundedBorder)
                .onSubmit { model.query(queryText) }
                .disabled(model.busy)

            if model.indexProgress != nil {
                IndexingProcessView()
            }

            if model.results.isEmpty {
                Spacer()
                Text(model.status)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .center)
                if !model.neededSecrets.isEmpty {
                    SecretPromptView()
                } else {
                    ForEach(model.parkedWarnings, id: \.self) { warning in
                        Text(warning)
                            .font(.caption)
                            .foregroundStyle(.orange)
                            .frame(maxWidth: .infinity, alignment: .center)
                    }
                    if (!model.nodeOpen || !model.parkedWarnings.isEmpty) && !model.busy {
                        Text("Open Settings (⌘,) to edit the composition or add an API key.")
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                            .frame(maxWidth: .infinity, alignment: .center)
                            .padding(.top, 4)
                    }
                }
                Spacer()
            } else {
                List(model.results) { result in
                    QueryResultRow(result: result)
                }
                .listStyle(.inset)
                Text(model.status)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding()
        .frame(minWidth: 560, minHeight: 420)
        .onAppear { model.openNode() }
    }
}

private struct IndexingProcessView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        if let progress = model.indexProgress {
            GroupBox {
                VStack(alignment: .leading, spacing: 9) {
                    HStack {
                        Text(progress.phase.label).font(.headline)
                        Spacer()
                        Text(countLabel(progress))
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                    if let fraction = progress.fractionCompleted {
                        ProgressView(value: fraction)
                    } else {
                        ProgressView()
                    }
                    if let current = progress.current {
                        Text(current)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                    HStack {
                        Text("\(progress.indexed) indexed · \(progress.unchanged) unchanged")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Spacer()
                        controls
                    }
                }
                .padding(.top, 3)
            } label: {
                Label("Indexing process", systemImage: "square.stack.3d.up")
            }
        }
    }

    @ViewBuilder
    private var controls: some View {
        if model.indexingActive {
            if model.indexStopping {
                ProgressView().controlSize(.small)
                Text("Stopping…").font(.caption)
            } else if model.indexPaused {
                Button("Resume") { model.resumeIndexing() }
                Button("Stop") { model.stopIndexing() }
            } else {
                Button("Pause") { model.pauseIndexing() }
                Button("Stop") { model.stopIndexing() }
            }
        } else {
            Button("Dismiss") { model.dismissIndexProgress() }
        }
    }

    private func countLabel(_ progress: IndexProgress) -> String {
        guard progress.sourcesTotal > 0 else { return "Discovering" }
        return "\(progress.sourcesComplete) of \(progress.sourcesTotal) sources"
    }
}
