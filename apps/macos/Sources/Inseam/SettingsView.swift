import AppKit
import SwiftUI

/// The template written when the node has no composition file yet. It only
/// carries comments: an empty file layers as a no-op over the built-in base.
private let compositionTemplate = """
# Node composition — patches the app's built-in base composition by entry id.
# The entries and their configs are documented in docs/configuration.md.
#
# Secrets never go in this file: an entry names an environment variable
# (for example the llm entry's `api_key_env`), and the Secrets tab stores
# that variable's value in your login Keychain.
#
# Example — fully offline node:
#
# [[entry]]
# id = "embedder"
# [entry.config]
# provider = "hashed"
# model = "hashed"
# dimensions = 256
#
# [[entry]]
# id = "llm"
# disabled = true
"""

/// Settings is file-first: the Composition tab edits the node's
/// `composition.toml` — the same file the CLI layers — and the Secrets tab
/// manages the Keychain items exported as environment variables at node
/// open. Saving either side reopens the node so changes take effect.
struct SettingsView: View {
    var body: some View {
        TabView {
            CompositionSettingsTab()
                .tabItem { Label("Composition", systemImage: "doc.text") }
            SecretsSettingsTab()
                .tabItem { Label("Secrets", systemImage: "key") }
        }
        .frame(width: 640, height: 440)
    }
}

struct CompositionSettingsTab: View {
    @EnvironmentObject private var model: AppModel
    @State private var text = ""
    @State private var message = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(model.compositionURL.path)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .lineLimit(1)
                    .truncationMode(.head)
                Spacer()
                Button("Reveal in Finder") {
                    NSWorkspace.shared.activateFileViewerSelecting([model.compositionURL])
                }
            }
            TextEditor(text: $text)
                .font(.system(.body, design: .monospaced))
                .autocorrectionDisabled()
                .frame(maxHeight: .infinity)
            HStack {
                Text(message)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Reload File") { load() }
                Button("Save & Reopen Node") { save() }
                    .keyboardShortcut("s", modifiers: .command)
            }
        }
        .padding()
        .onAppear { load() }
    }

    private func load() {
        if let loaded = try? String(contentsOf: model.compositionURL, encoding: .utf8) {
            text = loaded
            message = "loaded from disk"
        } else {
            text = compositionTemplate
            message = "no composition file yet — saving creates it"
        }
    }

    private func save() {
        do {
            try FileManager.default.createDirectory(
                at: model.dataDir, withIntermediateDirectories: true
            )
            try text.write(to: model.compositionURL, atomically: true, encoding: .utf8)
            message = "saved — reopening node"
            model.openNode()
        } catch {
            message = "save failed: \(error.localizedDescription)"
        }
    }
}

struct SecretsSettingsTab: View {
    @EnvironmentObject private var model: AppModel
    @State private var names: [String] = []
    @State private var newName = "OPENROUTER_API_KEY"
    @State private var newValue = ""
    @State private var message = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(
                "Secrets live in your login Keychain, never in the composition "
                    + "file. Each one is exported as an environment variable when "
                    + "the node opens, so the composition can reference it by name "
                    + "(the llm entry's `api_key_env`, for example)."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)

            List(names, id: \.self) { name in
                HStack {
                    Text(name).font(.body.monospaced())
                    Spacer()
                    Text("value hidden")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                    Button("Remove") { remove(name) }
                }
            }
            .frame(maxHeight: .infinity)

            Divider()

            HStack {
                TextField("ENV_VAR_NAME", text: $newName)
                    .font(.body.monospaced())
                    .frame(width: 220)
                SecureField("secret value", text: $newValue)
                Button("Save to Keychain") { save() }
                    .disabled(newValue.isEmpty)
            }
            Text(message)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding()
        .onAppear { refresh() }
    }

    private func refresh() {
        do {
            names = try SecretStore.names()
        } catch {
            message = error.localizedDescription
        }
    }

    private func save() {
        guard SecretStore.isValidName(newName) else {
            message = "`\(newName)` is not a valid environment variable name"
            return
        }
        do {
            try SecretStore.write(name: newName, value: newValue)
            newValue = ""
            message = "saved `\(newName)` — reopening node"
            refresh()
            model.openNode()
        } catch {
            message = error.localizedDescription
        }
    }

    private func remove(_ name: String) {
        do {
            try SecretStore.delete(name: name)
            unsetenv(name)
            message = "removed `\(name)` — reopening node"
            refresh()
            model.openNode()
        } catch {
            message = error.localizedDescription
        }
    }
}
