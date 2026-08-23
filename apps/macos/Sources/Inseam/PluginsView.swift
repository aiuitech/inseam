import AppKit
import SwiftUI

struct PluginsSettingsTab: View {
    @EnvironmentObject private var model: AppModel
    @State private var inspection: PluginDirectoryInspection?
    @State private var choosingDirectory = false
    @State private var selectionError: String?
    @State private var linkedExpanded = false

    private var loaded: [PluginView] {
        model.pluginEntries.filter { $0.plugin.hasPrefix("wasm:") }
    }

    private var linked: [PluginView] {
        model.pluginEntries.filter { !$0.plugin.hasPrefix("wasm:") }
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    pageHeader
                    loadedEntries
                    linkedEntries
                }
                .padding(24)
            }
            Divider()
            footer
        }
        .onAppear { model.refreshPlugins() }
        .onChange(of: model.nodeOpen) { _, open in
            if open { model.refreshPlugins() }
        }
        .sheet(item: $inspection) { inspection in
            PluginInstallSheet(inspection: inspection)
                .environmentObject(model)
        }
        .alert(
            "Cannot use plugin directory",
            isPresented: errorIsPresented,
            actions: { Button("OK") { selectionError = nil } },
            message: { Text(selectionError ?? "") }
        )
    }

    private var pageHeader: some View {
        HStack(alignment: .top, spacing: 20) {
            VStack(alignment: .leading, spacing: 5) {
                Text("Plugins").font(.title2.bold())
                Text("Mount a loaded plugin into the open node without restarting it.")
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if choosingDirectory {
                ProgressView().controlSize(.small)
            }
            Button("Install Plugin…") { chooseDirectory() }
                .disabled(model.pluginBusy || choosingDirectory || !model.nodeOpen)
        }
    }

    @ViewBuilder
    private var loadedEntries: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Loaded plugins").font(.headline)
            if loaded.isEmpty {
                GroupBox {
                    Text("No loaded plugins yet.")
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.vertical, 6)
                }
            } else {
                GroupBox {
                    VStack(spacing: 0) {
                        ForEach(Array(loaded.enumerated()), id: \.element.id) { index, entry in
                            PluginEntryRow(entry: entry)
                            if index + 1 < loaded.count { Divider() }
                        }
                    }
                }
            }
        }
    }

    private var linkedEntries: some View {
        GroupBox {
            DisclosureGroup(isExpanded: $linkedExpanded) {
                VStack(spacing: 0) {
                    ForEach(Array(linked.enumerated()), id: \.element.id) { index, entry in
                        PluginEntryRow(entry: entry)
                        if index + 1 < linked.count { Divider() }
                    }
                }
                .padding(.top, 8)
            } label: {
                Text("\(linked.count) linked entries")
                    .font(.headline)
            }
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Text(model.pluginMessage)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(2)
            Spacer()
            if model.pluginBusy { ProgressView().controlSize(.small) }
            Button("Reload") { model.refreshPlugins() }
                .disabled(model.pluginBusy || !model.nodeOpen)
        }
        .padding(.horizontal, 16)
        .frame(height: 52)
    }

    private var errorIsPresented: Binding<Bool> {
        Binding(
            get: { selectionError != nil },
            set: { presented in if !presented { selectionError = nil } }
        )
    }

    private func chooseDirectory() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "Choose the plugin's directory (its .wasm, manifest, checks, fixtures)"
        panel.prompt = "Choose"
        guard panel.runModal() == .OK else { return }
        guard let directory = panel.url else { return }
        choosingDirectory = true
        Task.detached(priority: .userInitiated) {
            do {
                let inspected = try CoreNode.inspectPluginDirectory(directory)
                await MainActor.run {
                    inspection = inspected
                    choosingDirectory = false
                }
            } catch {
                await MainActor.run {
                    selectionError = error.localizedDescription
                    choosingDirectory = false
                }
            }
        }
    }
}

private struct PluginEntryRow: View {
    let entry: PluginView

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: entry.plugin.hasPrefix("wasm:") ? "shippingbox.fill" : "link")
                .foregroundStyle(
                    entry.plugin.hasPrefix("wasm:")
                        ? Color.accentColor : Color(nsColor: .secondaryLabelColor)
                )
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 4) {
                Text(entry.id).font(.body.monospaced().bold())
                Text(entry.plugin)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .truncationMode(.middle)
                Text(entry.stateLabel)
                    .font(.caption)
                    .foregroundStyle(stateColor)
            }
            Spacer()
        }
        .padding(.vertical, 10)
    }

    private var stateColor: Color {
        switch entry.state {
        case .active: .secondary
        case .pending: .orange
        case .failed: .red
        }
    }
}

private struct PluginInstallSheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let inspection: PluginDirectoryInspection
    @State private var pluginId: String
    @State private var installError: String?

    init(inspection: PluginDirectoryInspection) {
        self.inspection = inspection
        _pluginId = State(initialValue: inspection.suggestedId)
    }

    private var idProblem: String? {
        CoreNode.pluginIdProblem(pluginId)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Install plugin").font(.title2.bold())
            directorySummary
            VStack(alignment: .leading, spacing: 6) {
                Text("Entry ID").font(.caption).foregroundStyle(.secondary)
                TextField("ocr", text: $pluginId)
                    .font(.body.monospaced())
                    .textFieldStyle(.roundedBorder)
                    .disabled(model.pluginBusy)
                if let idProblem {
                    Text(idProblem).font(.caption).foregroundStyle(.red)
                } else {
                    Text("Lowercase letters, digits, - and _, up to 64 characters.")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                }
            }
            HStack {
                if model.pluginBusy {
                    ProgressView().controlSize(.small)
                    Text("Installing and checking…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { dismiss() }.disabled(model.pluginBusy)
                Button("Install") { install() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(idProblem != nil || model.pluginBusy)
            }
        }
        .padding(24)
        .frame(width: 520)
        .interactiveDismissDisabled(model.pluginBusy)
        .alert(
            "Plugin installation failed",
            isPresented: errorIsPresented,
            actions: { Button("OK") { installError = nil } },
            message: { Text(installError ?? "") }
        )
    }

    private var directorySummary: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(inspection.directory.path)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(2)
                .truncationMode(.middle)
            Text(
                "\(inspection.files.count) files · "
                    + ByteCountFormatter.string(
                        fromByteCount: Int64(inspection.bytesTotal), countStyle: .file)
            )
            .font(.caption)
            .foregroundStyle(.tertiary)
        }
    }

    private var errorIsPresented: Binding<Bool> {
        Binding(
            get: { installError != nil },
            set: { presented in if !presented { installError = nil } }
        )
    }

    private func install() {
        model.installPlugin(id: pluginId, directory: inspection.directory) { error in
            if let error {
                installError = error
            } else {
                dismiss()
            }
        }
    }
}
