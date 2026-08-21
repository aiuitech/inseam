import AppKit
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

    let coreVersion = CoreNode.coreVersion()
    let dataDir: URL
    private var node: CoreNode?

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
        results = []
        status = "opening node…"
        let dataDir = dataDir
        run("node open") { [weak self] in
            old?.close()
            try SecretStore.exportIntoEnvironment()
            let node = try CoreNode(dataDir: dataDir)
            let health = try node.health()
            let parked = health.filter { $0.state != "active" }
            var seen = Set<String>()
            let needs = parked.flatMap(\.missingSecrets)
                .filter { seen.insert($0.env).inserted }
            let warnings = Self.parkedWarnings(in: health)
            return {
                self?.node = node
                self?.nodeOpen = true
                self?.parkedEntries = parked.map(\.id)
                self?.neededSecrets = needs
                self?.parkedWarnings = warnings
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
        run("index \(dir.lastPathComponent)") { [weak self] in
            let report = try node.indexDirectory(dir)
            return {
                self?.status = "indexed \(dir.lastPathComponent): "
                    + "\(report.indexed) indexed, \(report.unchanged) unchanged, "
                    + "\(report.fragments) fragments"
            }
        }
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
                    VStack(alignment: .leading, spacing: 3) {
                        HStack {
                            Text(result.envelope.title ?? result.address)
                                .font(.headline)
                                .lineLimit(1)
                            Spacer()
                            Text(String(format: "%.3f", result.score))
                                .font(.caption.monospacedDigit())
                                .foregroundStyle(.secondary)
                        }
                        Text(result.address)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                        if let summary = result.summary {
                            Text(summary).font(.callout).lineLimit(3)
                        }
                        Text("\(result.envelope.contentType) · \(result.envelope.length)")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                    .padding(.vertical, 2)
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
