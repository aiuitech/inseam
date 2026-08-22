import AppKit
import SwiftUI

private let compositionTemplate = """
    # Node composition patches the app's built-in defaults by entry id.
    # The visual Configuration tab manages every first-party entry.
    # Custom and loaded plugins can still be edited here.

    # Example:
    # [[entry]]
    # id = "ocr"
    # plugin = "wasm:plugins/ocr/ocr.wasm"
    """

enum SettingsTab: Hashable {
    case configuration
    case secrets
    case advanced
}

struct SettingsView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        TabView(selection: $model.settingsTab) {
            ConfigurationSettingsTab()
                .tabItem { Label("Configuration", systemImage: "slider.horizontal.3") }
                .tag(SettingsTab.configuration)
            SecretsSettingsTab()
                .tabItem { Label("Secrets", systemImage: "key") }
                .tag(SettingsTab.secrets)
            AdvancedSettingsTab()
                .tabItem { Label("Advanced", systemImage: "doc.text") }
                .tag(SettingsTab.advanced)
        }
        .frame(width: 820, height: 600)
    }
}

private enum ConfigurationCategory: String, CaseIterable, Identifiable {
    case sources
    case intelligence
    case indexing
    case search

    var id: String { rawValue }

    var title: String {
        switch self {
        case .sources: "Connections"
        case .intelligence: "Models"
        case .indexing: "Indexing"
        case .search: "Search"
        }
    }

    var icon: String {
        switch self {
        case .sources: "externaldrive"
        case .intelligence: "cpu"
        case .indexing: "square.stack.3d.up"
        case .search: "magnifyingglass"
        }
    }
}

struct ConfigurationSettingsTab: View {
    @EnvironmentObject private var model: AppModel
    @State private var category = ConfigurationCategory.sources
    @State private var settings: ConfigurationSettings?
    @State private var message = ""
    @State private var messageIsError = false

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 0) {
                categoryList
                Divider()
                detail
            }
            Divider()
            footer
        }
        .onAppear { load() }
    }

    private var categoryList: some View {
        List(ConfigurationCategory.allCases, selection: $category) { item in
            Label(item.title, systemImage: item.icon)
                .tag(item)
                .padding(.vertical, 4)
        }
        .listStyle(.sidebar)
        .frame(width: 170)
    }

    @ViewBuilder
    private var detail: some View {
        if let binding = Binding($settings) {
            ScrollView {
                Group {
                    switch category {
                    case .sources: SourceConfigurationView(settings: binding)
                    case .intelligence: ModelConfigurationView(settings: binding)
                    case .indexing: IndexConfigurationView(settings: binding)
                    case .search: SearchConfigurationView(settings: binding)
                    }
                }
                .padding(22)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            ContentUnavailableView(
                "Configuration unavailable",
                systemImage: "exclamationmark.triangle",
                description: Text(
                    message.isEmpty
                        ? "Loading the node configuration."
                        : "Fix the file in Advanced, then reload this view.\n\n\(message)"
                )
            )
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Text(model.compositionURL.path)
                .font(.caption)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
                .truncationMode(.head)
            Spacer()
            Text(message)
                .font(.caption)
                .foregroundStyle(messageIsError ? .red : .secondary)
                .lineLimit(2)
            Button("Reload") { load() }
            Button("Save Changes") { save() }
                .keyboardShortcut("s", modifiers: .command)
                .disabled(settings == nil)
        }
        .padding(.horizontal, 16)
        .frame(height: 52)
    }

    private func load() {
        do {
            settings = try CoreNode.readSettings(at: model.compositionURL)
            message = "Loaded"
            messageIsError = false
        } catch {
            settings = nil
            message = error.localizedDescription
            messageIsError = true
        }
    }

    private func save() {
        guard let settings else { return }
        do {
            try CoreNode.writeSettings(settings, to: model.compositionURL)
            message = "Saved. Reopening node…"
            messageIsError = false
            model.openNode()
        } catch {
            message = error.localizedDescription
            messageIsError = true
        }
    }
}

private struct SourceConfigurationView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        SettingsPage(
            title: "Connections",
            summary: "Configure local data access and reusable OAuth grants."
        ) {
            SettingsGroup(
                title: "Connection registry",
                summary: "Host connections register here so indexing can resolve them by host."
            ) {
                Toggle("Enabled", isOn: $settings.connections.enabled)
            }
            SettingsGroup(
                title: "Local filesystem",
                summary: "The filesystem host used when you choose a folder to index."
            ) {
                Toggle("Enabled", isOn: $settings.fs.enabled)
                Divider()
                VStack(alignment: .leading, spacing: 12) {
                    LabeledContent("Host ID") {
                        TextField("Automatic", text: optionalText($settings.fs.config.hostId))
                            .frame(width: 260)
                    }
                    Toggle("Skip hidden files and folders", isOn: $settings.fs.config.skipHidden)
                    Toggle("Honor .gitignore files", isOn: $settings.fs.config.gitignore)
                    VStack(alignment: .leading, spacing: 5) {
                        Text("Additional ignore patterns")
                        TextEditor(text: stringList($settings.fs.config.ignore))
                            .font(.body.monospaced())
                            .frame(height: 100)
                            .overlay {
                                RoundedRectangle(cornerRadius: 5)
                                    .stroke(.separator, lineWidth: 1)
                            }
                        Text("One gitignore-style pattern per line, anchored at the indexed root.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                .disabled(!settings.fs.enabled)
            }
            GoogleConnectionView(google: $settings.google)
            OAuthConfigurationView(oauth: $settings.oauth)
        }
    }
}

/// The Google Workspace connection: the entry's config, and — live from the
/// open node — where its grant stands, with the one button that fits.
private struct GoogleConnectionView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.openSettings) private var openSettings
    @Binding var google: Configurable<GoogleConnectionConfig>

    private var grant: GrantView? {
        model.grants.first { $0.id == google.config.grant }
    }

    private var googleHosts: [HostView] {
        model.hosts.filter { $0.entry == "google" }
    }

    var body: some View {
        SettingsGroup(
            title: "Google Workspace",
            summary:
                "One sign-in covers Gmail, Drive, Calendar, Contacts, and Tasks, each as its own host. The client id and secret come from your Keychain."
        ) {
            Toggle("Enabled", isOn: $google.enabled)
            Divider()
            connection
            Divider()
            VStack(spacing: 10) {
                SettingsTextField("Grant ID", text: $google.config.grant)
                SettingsTextField("Client ID variable", text: $google.config.clientIdEnv)
                SettingsTextField("Client secret variable", text: $google.config.clientSecretEnv)
                SettingsNumberField("Sources per service per run", value: $google.config.sourcesMax)
            }
            .disabled(!google.enabled)
            VStack(alignment: .leading, spacing: 6) {
                Text("Services").font(.caption).foregroundStyle(.secondary)
                ForEach(GoogleService.allCases) { service in
                    Toggle(service.label, isOn: serviceBinding(service))
                }
                Text("Leave the secret variable empty for a client without one. Changing the services or the variables needs Save Changes, then a fresh sign-in.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            .disabled(!google.enabled)
        }
    }

    @ViewBuilder
    private var connection: some View {
        if let grant {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text(statusLine(for: grant))
                        .font(.callout)
                    Spacer()
                    if model.authorizingGrant == grant.id {
                        ProgressView().controlSize(.small)
                        Text("Waiting for the browser…").font(.caption).foregroundStyle(.secondary)
                    } else if grant.state.isMissingSecret {
                        Button("Add Client ID…") {
                            model.suggestedSecretName = grant.state.env
                            model.settingsTab = .secrets
                            openSettings()
                        }
                    } else if grant.state.isAuthorized {
                        Button("Reconnect…") { model.authorize(grant: grant.id) }
                        Button("Disconnect") { model.revoke(grant: grant.id) }
                    } else {
                        Button("Connect Google…") { model.authorize(grant: grant.id) }
                    }
                }
                if !googleHosts.isEmpty {
                    ForEach(googleHosts) { host in
                        Text("\(host.displayName) · \(host.kind) · \(host.id)")
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                    }
                }
                if !model.connectionMessage.isEmpty {
                    Text(model.connectionMessage).font(.caption).foregroundStyle(.secondary)
                }
            }
        } else {
            Text(
                model.nodeOpen
                    ? "The node holds no grant `\(google.config.grant)` — save this entry enabled, then connect."
                    : "Open the node to connect."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
        }
    }

    private func statusLine(for grant: GrantView) -> String {
        switch grant.state.state {
        case "authorized":
            let account = grant.state.account.map { " as \($0)" } ?? ""
            return "Connected\(account)"
        case "missing_secret":
            return "Not connected — set \(grant.state.env ?? grant.clientIdEnv) in Secrets"
        default:
            return "Not connected"
        }
    }

    /// One service's toggle over the config's list, kept in catalog order.
    private func serviceBinding(_ service: GoogleService) -> Binding<Bool> {
        Binding(
            get: { google.config.services.contains(service.rawValue) },
            set: { enabled in
                var chosen = Set(google.config.services)
                if enabled {
                    chosen.insert(service.rawValue)
                } else {
                    chosen.remove(service.rawValue)
                }
                let ordered = GoogleService.allCases.map(\.rawValue)
                google.config.services = ordered.filter { chosen.contains($0) }
            }
        )
    }
}

private struct OAuthConfigurationView: View {
    @Binding var oauth: Configurable<OAuthConfig>

    var body: some View {
        SettingsGroup(
            title: "OAuth grants",
            summary:
                "Provider tokens stay in private credential files. Client secrets use Keychain variables."
        ) {
            Toggle("Enabled", isOn: $oauth.enabled)
            Divider()
            VStack(spacing: 12) {
                SettingsNumberField("Callback port", value: $oauth.config.callbackPort)
                SettingsNumberField(
                    "Authorization timeout (seconds)",
                    value: $oauth.config.authorizationTimeoutSecs
                )
                LabeledContent("Credentials directory") {
                    TextField(
                        "Application Support/inseam/oauth",
                        text: optionalText($oauth.config.credentialsDir)
                    )
                    .frame(width: 260)
                }
            }
            .disabled(!oauth.enabled)
            Divider()
            ForEach($oauth.config.grants) { $grant in
                OAuthGrantEditor(grant: $grant) {
                    oauth.config.grants.removeAll { $0.formId == grant.formId }
                }
                if grant.formId != oauth.config.grants.last?.formId { Divider() }
            }
            Button {
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
            } label: {
                Label("Add grant", systemImage: "plus")
            }
            .disabled(!oauth.enabled)
        }
    }
}

private struct OAuthGrantEditor: View {
    @Binding var grant: OAuthGrantConfig
    let remove: () -> Void

    var body: some View {
        DisclosureGroup {
            VStack(spacing: 10) {
                SettingsTextField("Grant ID", text: $grant.grantId)
                SettingsTextField("Authorization URL", text: $grant.authorizationUrl)
                SettingsTextField("Token URL", text: $grant.tokenUrl)
                SettingsTextField("Client ID variable", text: $grant.clientIdEnv)
                OptionalSettingsTextField("Client secret variable", text: $grant.clientSecretEnv)
                LabeledContent("Scopes") {
                    TextEditor(text: stringList($grant.scopes))
                        .font(.body.monospaced())
                        .frame(width: 260, height: 70)
                        .overlay {
                            RoundedRectangle(cornerRadius: 5).stroke(.separator, lineWidth: 1)
                        }
                }
                StringMapEditor(
                    title: "Authorization parameters", values: $grant.authorizationParams)
            }
            .padding(.top, 8)
        } label: {
            HStack {
                Text(grant.grantId).font(.body.monospaced())
                Spacer()
                Button(action: remove) { Image(systemName: "trash") }
                    .buttonStyle(.borderless)
                    .help("Remove grant")
            }
        }
    }
}

private struct StringMapEditor: View {
    let title: String
    @Binding var values: [String: String]
    @State private var newKey = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            ForEach(values.keys.sorted(), id: \.self) { key in
                HStack {
                    Text(key).font(.body.monospaced()).frame(width: 150, alignment: .leading)
                    TextField("Value", text: valueBinding(key))
                    Button {
                        values.removeValue(forKey: key)
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                    .buttonStyle(.borderless)
                }
            }
            HStack {
                TextField("parameter", text: $newKey).font(.body.monospaced())
                Button("Add") { add() }
                    .disabled(newKey.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }

    private func valueBinding(_ key: String) -> Binding<String> {
        Binding(get: { values[key] ?? "" }, set: { values[key] = $0 })
    }

    private func add() {
        let key = newKey.trimmingCharacters(in: .whitespaces)
        guard !key.isEmpty else { return }
        values[key] = values[key] ?? ""
        newKey = ""
    }
}

private struct ModelConfigurationView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        SettingsPage(
            title: "Models",
            summary:
                "Configure language and embedding models without putting credentials in the file."
        ) {
            languageModel
            embedder
        }
    }

    private var languageModel: some View {
        SettingsGroup(
            title: "Language model",
            summary: "An OpenAI-compatible endpoint used by summaries and entity extraction."
        ) {
            Toggle("Enabled", isOn: $settings.llm.enabled)
            Divider()
            VStack(spacing: 12) {
                SettingsTextField("Base URL", text: $settings.llm.config.baseUrl)
                SettingsTextField("API key variable", text: $settings.llm.config.apiKeyEnv)
                SettingsTextField("Transform model", text: $settings.llm.config.transformModel)
                SettingsTextField("Agent model", text: $settings.llm.config.agentModel)
            }
            .disabled(!settings.llm.enabled)
        }
    }

    private var embedder: some View {
        SettingsGroup(
            title: "Embeddings",
            summary: "Endpoint is semantic search. Hashed stays local. None uses full-text only."
        ) {
            Toggle("Enabled", isOn: $settings.embedder.enabled)
            Divider()
            VStack(spacing: 12) {
                LabeledContent("Provider") {
                    Picker("", selection: $settings.embedder.config.provider) {
                        ForEach(EmbedderProvider.allCases) { provider in
                            Text(provider.label).tag(provider)
                        }
                    }
                    .labelsHidden()
                    .frame(width: 260)
                }
                SettingsTextField("Model", text: $settings.embedder.config.model)
                SettingsNumberField("Dimensions", value: $settings.embedder.config.dimensions)
            }
            .disabled(!settings.embedder.enabled)
        }
    }
}

private struct IndexConfigurationView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        SettingsPage(
            title: "Indexing",
            summary: "Control the transform pipeline and the limits applied to each indexing run."
        ) {
            pipeline
            sweep
            ignoreRules
            operations
        }
    }

    private var pipeline: some View {
        SettingsGroup(
            title: "Transform pipeline",
            summary: "Disabled transforms stop contributing new fragments on the next sweep."
        ) {
            Toggle("Transform registry", isOn: $settings.transforms.enabled)
            Toggle("Markdown structure", isOn: $settings.markdown.enabled)
            Divider()
            TransformRow(
                title: "Plain-text chunking",
                enabled: $settings.chunker.enabled,
                firstLabel: "Target characters",
                firstValue: $settings.chunker.config.targetChars
            )
            TransformRow(
                title: "Summaries",
                enabled: $settings.summarizer.enabled,
                firstLabel: "Target characters",
                firstValue: $settings.summarizer.config.targetChars,
                budget: $settings.summarizer.config.llmCallBudget
            )
            TransformRow(
                title: "Entity extraction",
                enabled: $settings.entities.enabled,
                firstLabel: "Maximum per source",
                firstValue: $settings.entities.config.maxPerSource,
                budget: $settings.entities.config.llmCallBudget
            )
        }
    }

    private var sweep: some View {
        SettingsGroup(
            title: "Sweep limits",
            summary: "Limits bound work and file size. Zero maximum sources means unlimited."
        ) {
            Toggle("Enabled", isOn: $settings.sweep.enabled)
            Divider()
            VStack(spacing: 12) {
                SettingsNumberField("Maximum sources", value: $settings.sweep.config.maxSources)
                SettingsNumberField("Concurrent sources", value: $settings.sweep.config.concurrency)
                SettingsNumberField(
                    "Fragments per source",
                    value: $settings.sweep.config.maxFragmentsPerSource
                )
                SettingsNumberField("Maximum depth", value: $settings.sweep.config.maxDepth)
                SettingsNumberField(
                    "Maximum content bytes",
                    value: $settings.sweep.config.maxContentBytes
                )
                LabeledContent("Modified on or after") {
                    TextField(
                        "YYYY-MM-DD",
                        text: optionalText($settings.sweep.config.modifiedAfter)
                    )
                    .frame(width: 260)
                }
            }
            .disabled(!settings.sweep.enabled)
        }
    }

    private var ignoreRules: some View {
        SettingsGroup(
            title: "Source ignore rules",
            summary: "A source is ignored when every field set on one rule matches."
        ) {
            ForEach($settings.sweep.config.ignore) { $rule in
                IgnoreRuleEditor(rule: $rule) {
                    settings.sweep.config.ignore.removeAll { $0.id == rule.id }
                }
                if rule.id != settings.sweep.config.ignore.last?.id { Divider() }
            }
            Button {
                settings.sweep.config.ignore.append(
                    IgnoreRule(contentType: "application/octet-stream")
                )
            } label: {
                Label("Add rule", systemImage: "plus")
            }
            .disabled(!settings.sweep.enabled)
        }
    }

    private var operations: some View {
        SettingsGroup(
            title: "App operations",
            summary: "Search and indexing in this app require the operations service."
        ) {
            Toggle("Enabled", isOn: $settings.operations.enabled)
        }
    }
}

private struct SearchConfigurationView: View {
    @Binding var settings: ConfigurationSettings

    var body: some View {
        SettingsPage(
            title: "Search",
            summary: "Tune seed retrieval, graph expansion, and relation weights."
        ) {
            ranking
            RelationWeightsEditor(weights: $settings.finder.config.weights)
                .disabled(!settings.finder.enabled)
        }
    }

    private var ranking: some View {
        SettingsGroup(
            title: "Finder",
            summary: "These settings affect queries immediately and do not rebuild the index."
        ) {
            Toggle("Enabled", isOn: $settings.finder.enabled)
            Divider()
            VStack(spacing: 12) {
                SettingsNumberField("Seed results", value: $settings.finder.config.seedK)
                SettingsNumberField("RRF constant", value: $settings.finder.config.rrfK)
                SettingsNumberField("Graph damping", value: $settings.finder.config.damping)
                SettingsNumberField("Iterations", value: $settings.finder.config.iterations)
                SettingsNumberField("Early-exit epsilon", value: $settings.finder.config.epsilon)
                SettingsNumberField("Hints per result", value: $settings.finder.config.maxHints)
                SettingsNumberField(
                    "Maximum vector distance",
                    value: $settings.finder.config.maxVectorDistance
                )
            }
            .disabled(!settings.finder.enabled)
        }
    }
}

private struct RelationWeightsEditor: View {
    @Binding var weights: RelationWeights
    @State private var newKind = ""

    var body: some View {
        SettingsGroup(
            title: "Relation weights",
            summary: "Higher values let that relation carry more relevance through the graph."
        ) {
            SettingsNumberField("Unlisted relations", value: $weights.default)
            Divider()
            ForEach(weights.byKind.keys.sorted(), id: \.self) { kind in
                HStack {
                    Text(kind).font(.body.monospaced())
                    Spacer()
                    TextField("Weight", value: weightBinding(kind), format: .number)
                        .multilineTextAlignment(.trailing)
                        .frame(width: 90)
                    Button {
                        weights.byKind.removeValue(forKey: kind)
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                    .buttonStyle(.borderless)
                    .help("Remove \(kind)")
                }
            }
            HStack {
                TextField("relation-kind", text: $newKind)
                    .font(.body.monospaced())
                Button("Add") { addKind() }
                    .disabled(newKind.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }

    private func weightBinding(_ kind: String) -> Binding<Double> {
        Binding(
            get: { weights.byKind[kind] ?? 0.5 },
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

private struct IgnoreRuleEditor: View {
    @Binding var rule: IgnoreRule
    let remove: () -> Void

    var body: some View {
        DisclosureGroup {
            VStack(spacing: 8) {
                OptionalSettingsTextField("Address", text: $rule.address)
                OptionalSettingsTextField("Host", text: $rule.host)
                OptionalSettingsTextField("Locator", text: $rule.locator)
                OptionalSettingsTextField("Source type", text: $rule.sourceType)
                OptionalSettingsTextField("Content type", text: $rule.contentType)
                OptionalSettingsTextField("Hint", text: $rule.hint)
                OptionalSettingsTextField("Property", text: $rule.property)
            }
            .padding(.top, 8)
        } label: {
            HStack {
                Text(ruleSummary)
                    .font(.body.monospaced())
                    .lineLimit(1)
                Spacer()
                Button(action: remove) {
                    Image(systemName: "trash")
                }
                .buttonStyle(.borderless)
                .help("Remove rule")
            }
        }
    }

    private var ruleSummary: String {
        let values = [
            rule.address, rule.host, rule.locator, rule.sourceType,
            rule.contentType, rule.hint, rule.property,
        ].compactMap { $0 }.filter { !$0.isEmpty }
        return values.isEmpty ? "Empty rule" : values.joined(separator: " + ")
    }
}

private struct TransformRow: View {
    let title: String
    @Binding var enabled: Bool
    let firstLabel: String
    @Binding var firstValue: Int
    var budget: Binding<Int>?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Toggle(title, isOn: $enabled)
            HStack(spacing: 18) {
                TextField(firstLabel, value: $firstValue, format: .number)
                if let budget {
                    TextField("LLM calls per run", value: budget, format: .number)
                }
            }
            .textFieldStyle(.roundedBorder)
            .disabled(!enabled)
        }
        .padding(.vertical, 4)
    }
}

private struct SettingsPage<Content: View>: View {
    let title: String
    let summary: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.title2.bold())
                Text(summary).font(.callout).foregroundStyle(.secondary)
            }
            content
        }
        .frame(maxWidth: 590, alignment: .leading)
        .frame(maxWidth: .infinity, alignment: .top)
    }
}

private struct SettingsGroup<Content: View>: View {
    let title: String
    let summary: String
    @ViewBuilder let content: Content

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 10) { content }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 4)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                Text(summary).font(.caption).foregroundStyle(.secondary)
            }
            .padding(.bottom, 4)
        }
    }
}

private struct SettingsTextField: View {
    let label: String
    @Binding var text: String

    init(_ label: String, text: Binding<String>) {
        self.label = label
        _text = text
    }

    var body: some View {
        LabeledContent(label) {
            TextField(label, text: $text).frame(width: 260)
        }
    }
}

private struct OptionalSettingsTextField: View {
    let label: String
    @Binding var text: String?

    init(_ label: String, text: Binding<String?>) {
        self.label = label
        _text = text
    }

    var body: some View {
        LabeledContent(label) {
            TextField("Any", text: optionalText($text)).frame(width: 260)
        }
    }
}

private struct SettingsNumberField<Value: ParseableFormatStyle>: View
where Value.FormatInput: Equatable, Value.FormatOutput == String {
    let label: String
    @Binding var value: Value.FormatInput
    let format: Value

    init(_ label: String, value: Binding<Int>) where Value == IntegerFormatStyle<Int> {
        self.label = label
        _value = value
        format = .number
    }

    init(_ label: String, value: Binding<UInt64>) where Value == IntegerFormatStyle<UInt64> {
        self.label = label
        _value = value
        format = .number
    }

    init(_ label: String, value: Binding<Double>) where Value == FloatingPointFormatStyle<Double> {
        self.label = label
        _value = value
        format = .number.precision(.significantDigits(1...8))
    }

    var body: some View {
        LabeledContent(label) {
            TextField(label, value: $value, format: format)
                .multilineTextAlignment(.trailing)
                .frame(width: 120)
        }
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

struct AdvancedSettingsTab: View {
    @EnvironmentObject private var model: AppModel
    @State private var text = ""
    @State private var message = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Raw composition").font(.headline)
                    Text("For custom plugins and settings the visual editor does not know about.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
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
                Text(message).font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Reload File") { load() }
                Button("Save & Reopen Node") { save() }
            }
        }
        .padding()
        .onAppear { load() }
    }

    private func load() {
        if let loaded = try? String(contentsOf: model.compositionURL, encoding: .utf8) {
            text = loaded
            message = "Loaded from disk"
        } else {
            text = compositionTemplate
            message = "No composition file yet. Saving creates it."
        }
    }

    private func save() {
        do {
            try FileManager.default.createDirectory(
                at: model.dataDir, withIntermediateDirectories: true
            )
            try text.write(to: model.compositionURL, atomically: true, encoding: .utf8)
            message = "Saved. Reopening node…"
            model.openNode()
        } catch {
            message = "Save failed: \(error.localizedDescription)"
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
            Text("API keys").font(.title2.bold())
            Text(
                "Secrets stay in your login Keychain. The configuration stores only the "
                    + "environment variable name used when the node opens."
            )
            .font(.callout)
            .foregroundStyle(.secondary)

            List(names, id: \.self) { name in
                HStack {
                    Text(name).font(.body.monospaced())
                    Spacer()
                    Text("Value hidden").font(.caption).foregroundStyle(.tertiary)
                    Button("Remove") { remove(name) }
                }
            }

            Divider()
            HStack {
                TextField("ENV_VAR_NAME", text: $newName)
                    .font(.body.monospaced())
                    .frame(width: 220)
                SecureField("Secret value", text: $newValue)
                Button("Save to Keychain") { save() }.disabled(newValue.isEmpty)
            }
            Text(message).font(.caption).foregroundStyle(.secondary)
        }
        .padding()
        .onAppear {
            refresh()
            if let suggested = model.suggestedSecretName {
                newName = suggested
                model.suggestedSecretName = nil
            }
        }
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
            message = "Saved `\(newName)`. Reopening node…"
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
            message = "Removed `\(name)`. Reopening node…"
            refresh()
            model.openNode()
        } catch {
            message = error.localizedDescription
        }
    }
}
