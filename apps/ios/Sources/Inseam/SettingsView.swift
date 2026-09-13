import InseamKit
import SwiftUI

struct SettingsView: View {
    @Environment(NodeModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section("node") {
                    ConfigurationSettingsView()
                    SecretsSettingsView()
                    AdvancedSettingsView()
                }
                Section("hosted node") {
                    HostedNodeSettingsView()
                }
                Section {
                    LabeledContent("core", value: model.coreVersion)
                    Text(model.dataDir.path)
                        .font(Brand.font(.caption2))
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle("settings")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("done") { dismiss() }
                }
            }
        }
        .preferredColorScheme(.dark)
        .font(Brand.font(.body))
    }
}

private struct ConfigurationSettingsView: View {
    @Environment(NodeModel.self) private var model
    @State private var settings: ConfigurationSettings?
    @State private var message = ""

    var body: some View {
        NavigationLink("configuration") {
            Group {
                if let settingsBinding = Binding($settings) {
                    List {
                        NavigationLink("connections") {
                            ConnectionSettingsView(settings: settingsBinding)
                        }
                        NavigationLink("models") {
                            ModelSettingsView(settings: settingsBinding)
                        }
                        NavigationLink("indexing") {
                            IndexSettingsView(settings: settingsBinding)
                        }
                        NavigationLink("search") {
                            SearchSettingsView(settings: settingsBinding)
                        }
                        if !message.isEmpty {
                            Text(message).font(Brand.font(.caption)).foregroundStyle(.secondary)
                        }
                    }
                    .navigationTitle("configuration")
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("save") { save() }.disabled(model.busy)
                        }
                    }
                } else {
                    ContentUnavailableView(
                        "configuration unavailable",
                        systemImage: "exclamationmark.triangle",
                        description: Text(message)
                    )
                    .navigationTitle("configuration")
                }
            }
            .task { load() }
        }
    }

    private func load() {
        do {
            settings = try CoreNode.readSettings(at: model.compositionURL)
            message = "Loaded"
        } catch {
            settings = nil
            message = error.localizedDescription
        }
    }

    private func save() {
        guard let settings else { return }
        do {
            try CoreNode.writeSettings(
                settings,
                to: model.compositionURL,
                scope: .preservingShellEmbedder
            )
            message = "Saved. Reopening node…"
            model.openNode()
        } catch {
            message = error.localizedDescription
        }
    }
}

private struct ConnectionSettingsView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        Form {
            Section("connection registry") {
                Toggle("enabled", isOn: $settings.connections.enabled)
            }
            Section("local filesystem") {
                Toggle("enabled", isOn: $settings.fs.enabled)
                labeledTextField(
                    "machine identity (automatic)",
                    text: optionalText($settings.fs.config.machineId)
                )
                Toggle("skip hidden files and folders", isOn: $settings.fs.config.skipHidden)
                Toggle("honor .gitignore files", isOn: $settings.fs.config.gitignore)
                VStack(alignment: .leading) {
                    Text("folders to index").font(Brand.font(.caption))
                    TextEditor(text: stringList($settings.fs.config.roots)).frame(minHeight: 70)
                }
                VStack(alignment: .leading) {
                    Text("additional ignore patterns").font(Brand.font(.caption))
                    TextEditor(text: stringList($settings.fs.config.ignore)).frame(minHeight: 90)
                }
            }
            GoogleSettingsSection(google: $settings.google)
            OAuthSettingsSection(oauth: $settings.oauth)
        }
        .navigationTitle("connections")
    }
}

private struct GoogleSettingsSection: View {
    @Binding var google: Configurable<GoogleConnectionConfig>

    var body: some View {
        Section("Google Workspace") {
            Toggle("enabled", isOn: $google.enabled)
            labeledTextField("grant id", text: $google.config.grant)
            labeledTextField("client id variable", text: $google.config.clientIdEnv)
                .textInputAutocapitalization(.characters)
            labeledTextField("client secret variable", text: $google.config.clientSecretEnv)
                .textInputAutocapitalization(.characters)
            numberField("sources per service", value: $google.config.sourcesMax)
            ForEach(GoogleService.allCases) { service in
                Toggle(service.label, isOn: serviceBinding(service))
            }
        }
    }

    private func serviceBinding(_ service: GoogleService) -> Binding<Bool> {
        Binding(
            get: { google.config.services.contains(service.rawValue) },
            set: { enabled in
                var selected = Set(google.config.services)
                if enabled {
                    selected.insert(service.rawValue)
                } else {
                    selected.remove(service.rawValue)
                }
                google.config.services = GoogleService.allCases.map(\.rawValue)
                    .filter { selected.contains($0) }
            }
        )
    }
}

private struct OAuthSettingsSection: View {
    @Binding var oauth: Configurable<OAuthConfig>

    var body: some View {
        Section("OAuth") {
            Toggle("enabled", isOn: $oauth.enabled)
            numberField("callback port", value: $oauth.config.callbackPort)
            ConfigField("authorization timeout seconds") {
                TextField(
                    "Value",
                    value: $oauth.config.authorizationTimeoutSecs,
                    format: .number
                )
                .keyboardType(.numberPad)
            }
            labeledTextField(
                "credentials directory",
                text: optionalText($oauth.config.credentialsDir)
            )
            ForEach($oauth.config.grants) { $grant in
                DisclosureGroup(grant.grantId) {
                    labeledTextField("grant id", text: $grant.grantId)
                    labeledTextField("authorization URL", text: $grant.authorizationUrl)
                    labeledTextField("token URL", text: $grant.tokenUrl)
                    labeledTextField("client id variable", text: $grant.clientIdEnv)
                    labeledTextField(
                        "client secret variable",
                        text: optionalText($grant.clientSecretEnv)
                    )
                    ConfigField("scopes, one per line") {
                        TextEditor(text: stringList($grant.scopes)).frame(minHeight: 80)
                    }
                    OAuthParametersEditor(parameters: $grant.authorizationParams)
                    Button("remove grant", role: .destructive) {
                        oauth.config.grants.removeAll { $0.formId == grant.formId }
                    }
                }
            }
            Button("add grant") {
                oauth.config.grants.append(
                    OAuthGrantConfig(
                        grantId: "new-grant",
                        authorizationUrl: "https://provider.example/authorize",
                        tokenUrl: "https://provider.example/token",
                        scopes: [],
                        clientIdEnv: "OAUTH_CLIENT_ID",
                        clientSecretEnv: nil,
                        authorizationParams: [:]
                    )
                )
            }
        }
    }
}

private struct OAuthParametersEditor: View {
    @Binding var parameters: [String: String]
    @State private var newName = ""
    @State private var newValue = ""

    var body: some View {
        Group {
            Text("authorization parameters").font(Brand.font(.caption))
            ForEach(parameters.keys.sorted(), id: \.self) { name in
                HStack {
                    Text(name)
                    TextField("value", text: value(name))
                    Button(role: .destructive) {
                        parameters.removeValue(forKey: name)
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                }
            }
            HStack {
                TextField("parameter", text: $newName)
                TextField("value", text: $newValue)
                Button("add") { addParameter() }
            }
        }
    }

    private func value(_ name: String) -> Binding<String> {
        Binding(
            get: { parameters[name] ?? "" },
            set: { parameters[name] = $0 }
        )
    }

    private func addParameter() {
        let name = newName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return }
        parameters[name] = newValue
        newName = ""
        newValue = ""
    }
}

private struct ModelSettingsView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        Form {
            Section("language model") {
                Toggle("enabled", isOn: $settings.llm.enabled)
                labeledTextField("base URL", text: $settings.llm.config.baseUrl)
                labeledTextField("API key variable", text: $settings.llm.config.apiKeyEnv)
                labeledTextField("transform model", text: $settings.llm.config.transformModel)
                labeledTextField(
                    "transform reasoning effort",
                    text: optionalText($settings.llm.config.transformReasoningEffort)
                )
                labeledTextField("agent model", text: $settings.llm.config.agentModel)
            }
            Section("embeddings") {
                LabeledContent("provider", value: "Apple on-device")
                Text(
                    "The visual editor keeps the phone's on-device embedder. "
                        + "Use raw composition only when you intend to replace it."
                )
                .font(Brand.font(.caption))
                .foregroundStyle(.secondary)
            }
        }
        .navigationTitle("models")
    }
}

private struct IndexSettingsView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        Form {
            Section("transform pipeline") {
                Toggle("transform registry", isOn: $settings.transforms.enabled)
                Toggle("markdown structure", isOn: $settings.markdown.enabled)
                Toggle("plain-text chunking", isOn: $settings.chunker.enabled)
                numberField("chunk target characters", value: $settings.chunker.config.targetChars)
                Toggle("summaries", isOn: $settings.summarizer.enabled)
                numberField("summary target characters", value: $settings.summarizer.config.targetChars)
                numberField("summary LLM calls", value: $settings.summarizer.config.llmCallBudget)
                Toggle("entity extraction", isOn: $settings.entities.enabled)
                numberField("entities per source", value: $settings.entities.config.maxPerSource)
                numberField("entity LLM calls", value: $settings.entities.config.llmCallBudget)
            }
            SweepSettingsSection(sweep: $settings.sweep)
            IgnoreSettingsSection(rules: $settings.sweep.config.ignore)
            Section("app operations") {
                Toggle("enabled", isOn: $settings.operations.enabled)
            }
        }
        .navigationTitle("indexing")
    }
}

private struct SweepSettingsSection: View {
    @Binding var sweep: Configurable<SweepConfig>

    var body: some View {
        Section {
            Toggle("enabled", isOn: $sweep.enabled)
            numberField("maximum sources", value: $sweep.config.maxSources)
            numberField("concurrent sources", value: $sweep.config.concurrency)
            numberField("concurrent batch sources", value: $sweep.config.batchConcurrency)
            numberField("concurrent source reads", value: $sweep.config.sourceReadsInFlightMax)
            numberField("fragments per source", value: $sweep.config.maxFragmentsPerSource)
            numberField("maximum depth", value: $sweep.config.maxDepth)
            ConfigField("maximum content bytes") {
                TextField("Value", value: $sweep.config.maxContentBytes, format: .number)
                    .keyboardType(.numberPad)
            }
            numberField("reference follow depth", value: $sweep.config.maxReferenceHops)
            labeledTextField(
                "modified on or after",
                text: optionalText($sweep.config.modifiedAfter),
                prompt: "YYYY-MM-DD or empty"
            )
        } header: {
            Text("sweep limits")
        } footer: {
            Text("Maximum sources uses 0 for unlimited. Dates use YYYY-MM-DD.")
        }
    }
}

private struct IgnoreSettingsSection: View {
    @Binding var rules: [IgnoreRule]

    var body: some View {
        Section("source ignore rules") {
            ForEach($rules) { $rule in
                DisclosureGroup(ruleLabel(rule)) {
                    labeledTextField("address", text: optionalText($rule.address))
                    labeledTextField("host", text: optionalText($rule.host))
                    labeledTextField("locator", text: optionalText($rule.locator))
                    labeledTextField("source type", text: optionalText($rule.sourceType))
                    labeledTextField("content type", text: optionalText($rule.contentType))
                    labeledTextField("hint", text: optionalText($rule.hint))
                    labeledTextField("property", text: optionalText($rule.property))
                    Button("remove rule", role: .destructive) {
                        rules.removeAll { $0.id == rule.id }
                    }
                }
            }
            Button("add rule") {
                rules.append(IgnoreRule(contentType: "application/octet-stream"))
            }
        }
    }

    private func ruleLabel(_ rule: IgnoreRule) -> String {
        let values = [rule.address, rule.host, rule.locator, rule.contentType]
            .compactMap { $0 }
            .filter { !$0.isEmpty }
        return values.isEmpty ? "empty rule" : values.joined(separator: " + ")
    }
}

private struct SearchSettingsView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        Form {
            Section("finder") {
                Toggle("enabled", isOn: $settings.finder.enabled)
                numberField("seed results", value: $settings.finder.config.seedK)
                decimalField("RRF constant", value: $settings.finder.config.rrfK)
                decimalField("graph damping", value: $settings.finder.config.damping)
                numberField("iterations", value: $settings.finder.config.iterations)
                decimalField("early-exit epsilon", value: $settings.finder.config.epsilon)
                numberField("hints per result", value: $settings.finder.config.maxHints)
                decimalField(
                    "maximum vector distance",
                    value: $settings.finder.config.maxVectorDistance
                )
            }
            RelationWeightsSection(weights: $settings.finder.config.weights)
        }
        .navigationTitle("search")
    }
}

private struct RelationWeightsSection: View {
    @Binding var weights: RelationWeights
    @State private var newKind = ""

    var body: some View {
        Section("relation weights") {
            decimalField("unlisted relations", value: $weights.default)
            ForEach(weights.byKind.keys.sorted(), id: \.self) { kind in
                HStack {
                    Text(kind)
                    Spacer()
                    TextField("weight", value: weight(kind), format: .number)
                        .multilineTextAlignment(.trailing)
                    Button(role: .destructive) {
                        weights.byKind.removeValue(forKey: kind)
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                }
            }
            HStack {
                TextField("relation-kind", text: $newKind)
                Button("add") { addKind() }
            }
        }
    }

    private func weight(_ kind: String) -> Binding<Double> {
        Binding(
            get: { weights.byKind[kind] ?? weights.default },
            set: { weights.byKind[kind] = $0 }
        )
    }

    private func addKind() {
        let kind = newKind.trimmingCharacters(in: .whitespaces)
        guard !kind.isEmpty else { return }
        weights.byKind[kind] = weights.byKind[kind] ?? weights.default
        newKind = ""
    }
}

private struct SecretsSettingsView: View {
    var body: some View {
        NavigationLink("secrets") { SecretsEditor() }
    }
}

private struct SecretsEditor: View {
    @Environment(NodeModel.self) private var model
    @State private var names: [String] = []
    @State private var name = "OPENROUTER_API_KEY"
    @State private var value = ""
    @State private var message = ""

    var body: some View {
        Form {
            Section("stored in Keychain") {
                ForEach(names, id: \.self) { stored in
                    HStack {
                        Text(stored).font(Brand.font(.caption))
                        Spacer()
                        Button("remove", role: .destructive) { remove(stored) }
                            .disabled(model.busy)
                    }
                }
            }
            Section("add secret") {
                labeledTextField("environment variable", text: $name, prompt: "ENV_VAR_NAME")
                    .textInputAutocapitalization(.characters)
                    .autocorrectionDisabled()
                ConfigField("secret value") {
                    SecureField("Value", text: $value)
                }
                Button("save to Keychain") { save() }
                    .disabled(value.isEmpty || model.busy)
            }
            if !message.isEmpty {
                Text(message).font(Brand.font(.caption)).foregroundStyle(.secondary)
            }
        }
        .navigationTitle("secrets")
        .task { refresh() }
    }

    private func refresh() {
        do {
            names = try SecretStore.names()
        } catch {
            message = error.localizedDescription
        }
    }

    private func save() {
        guard SecretStore.isValidName(name) else {
            message = "`\(name)` is not a valid environment variable name"
            return
        }
        do {
            try SecretStore.write(name: name, value: value)
            value = ""
            refresh()
            model.openNode()
            message = "Saved. Reopening node…"
        } catch {
            message = error.localizedDescription
        }
    }

    private func remove(_ stored: String) {
        do {
            try SecretStore.delete(name: stored)
            unsetenv(stored)
            refresh()
            model.openNode()
            message = "Removed. Reopening node…"
        } catch {
            message = error.localizedDescription
        }
    }
}

/// Where the phone finds the owner's always-on node for call capture.
/// The URL is a preference; the owner token is a Keychain secret like
/// every other secret the app holds.
private struct HostedNodeSettingsView: View {
    @Environment(NodeModel.self) private var model
    @State private var url = UserDefaults.standard.string(forKey: HostedNodeClient.baseURLDefaultsKey) ?? ""
    @State private var token = ""
    @State private var message = ""

    var body: some View {
        NavigationLink("hosted node") {
            Form {
                Section {
                    labeledTextField("node URL", text: $url, prompt: "https://you.inseam.io")
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    ConfigField("owner token") {
                        SecureField("from the claim page", text: $token)
                    }
                    Button("save") { save() }
                        .disabled(url.isEmpty || model.busy)
                } footer: {
                    Text("Call capture is placed by this node: it owns the phone line and keeps the recordings. Leave the token blank to keep the one already saved.")
                        .font(Brand.font(.caption2))
                }
                if !message.isEmpty {
                    Text(message).font(Brand.font(.caption)).foregroundStyle(.secondary)
                }
            }
            .navigationTitle("hosted node")
        }
    }

    private func save() {
        guard let parsed = URL(string: url.trimmingCharacters(in: .whitespaces)), parsed.host != nil else {
            message = "enter the node's full URL, scheme included"
            return
        }
        do {
            UserDefaults.standard.set(parsed.absoluteString, forKey: HostedNodeClient.baseURLDefaultsKey)
            if !token.isEmpty {
                try SecretStore.write(name: HostedNodeClient.ownerTokenSecretName, value: token)
                token = ""
            }
            model.reloadHostedNode()
            message = model.hostedNode == nil ? "saved the URL; an owner token is still needed" : "saved"
        } catch {
            message = "save failed: \(error.localizedDescription)"
        }
    }
}

private struct AdvancedSettingsView: View {
    var body: some View {
        NavigationLink("raw composition") { AdvancedEditor() }
    }
}

private struct AdvancedEditor: View {
    @Environment(NodeModel.self) private var model
    @State private var text = ""
    @State private var message = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            TextEditor(text: $text)
                .font(Brand.font(.caption))
                .autocorrectionDisabled()
            Text(message).font(Brand.font(.caption)).foregroundStyle(.secondary)
        }
        .padding()
        .navigationTitle("raw composition")
        .toolbar {
            ToolbarItem(placement: .confirmationAction) {
                Button("save") { save() }.disabled(model.busy)
            }
        }
        .task { load() }
    }

    private func load() {
        do {
            text = try String(contentsOf: model.compositionURL, encoding: .utf8)
            message = "Loaded from disk"
        } catch {
            text = "# Node composition patches the app defaults by entry id.\n"
            message = "No composition file yet. Saving creates it."
        }
    }

    private func save() {
        do {
            try FileManager.default.createDirectory(
                at: model.dataDir,
                withIntermediateDirectories: true
            )
            try text.write(to: model.compositionURL, atomically: true, encoding: .utf8)
            model.openNode()
            message = "Saved. Reopening node…"
        } catch {
            message = error.localizedDescription
        }
    }
}

private func numberField(_ label: String, value: Binding<Int>) -> some View {
    ConfigField(label) {
        TextField("Value", value: value, format: .number).keyboardType(.numberPad)
    }
}

private func decimalField(_ label: String, value: Binding<Double>) -> some View {
    ConfigField(label) {
        TextField("Value", value: value, format: .number).keyboardType(.decimalPad)
    }
}

private func labeledTextField(
    _ label: String,
    text: Binding<String>,
    prompt: String = "Not set"
) -> some View {
    ConfigField(label) {
        TextField("", text: text, prompt: Text(prompt))
            .accessibilityLabel(label)
    }
}

private struct ConfigField<Content: View>: View {
    let label: String
    let content: Content

    init(_ label: String, @ViewBuilder content: () -> Content) {
        self.label = label
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(label)
                .font(Brand.font(.caption2))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
            content
                .font(Brand.font(.body))
                .accessibilityLabel(label)
        }
        .padding(.vertical, 2)
    }
}

private func optionalText(_ value: Binding<String?>) -> Binding<String> {
    Binding(
        get: { value.wrappedValue ?? "" },
        set: { value.wrappedValue = $0.isEmpty ? nil : $0 }
    )
}

private func stringList(_ value: Binding<[String]>) -> Binding<String> {
    Binding(
        get: { value.wrappedValue.joined(separator: "\n") },
        set: {
            value.wrappedValue = $0.split(separator: "\n", omittingEmptySubsequences: true)
                .map(String.init)
        }
    )
}
