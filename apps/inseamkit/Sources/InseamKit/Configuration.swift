import Foundation

public struct ConfigurationSettings: Codable {
    public var connections: FeatureToggle
    public var fs: Configurable<FilesystemConfig>
    public var oauth: Configurable<OAuthConfig>
    public var google: Configurable<GoogleConnectionConfig>
    public var llm: Configurable<LanguageModelConfig>
    public var embedder: Configurable<EmbedderConfig>
    public var transforms: FeatureToggle
    public var markdown: FeatureToggle
    public var chunker: Configurable<ChunkerConfig>
    public var summarizer: Configurable<SummarizerConfig>
    public var entities: Configurable<EntityConfig>
    public var finder: Configurable<FinderConfig>
    public var sweep: Configurable<SweepConfig>
    public var operations: FeatureToggle
}

public struct OAuthConfig: Codable {
    public var callbackPort: Int
    public var authorizationTimeoutSecs: UInt64
    public var credentialsDir: String?
    public var grants: [OAuthGrantConfig]
}

public struct OAuthGrantConfig: Codable, Identifiable {
    public var formId = UUID()
    public var grantId: String
    public var authorizationUrl: String
    public var tokenUrl: String
    public var scopes: [String]
    public var clientIdEnv: String
    public var clientSecretEnv: String?
    public var authorizationParams: [String: String]

    enum CodingKeys: String, CodingKey {
        case grantId = "id"
        case authorizationUrl, tokenUrl, scopes, clientIdEnv, clientSecretEnv
        case authorizationParams
    }

    public var id: UUID { formId }

    /// The memberwise initializer, spelled out because a public struct's
    /// synthesized one stays internal; the apps build a fresh grant from it.
    public init(
        grantId: String,
        authorizationUrl: String,
        tokenUrl: String,
        scopes: [String],
        clientIdEnv: String,
        clientSecretEnv: String?,
        authorizationParams: [String: String]
    ) {
        self.grantId = grantId
        self.authorizationUrl = authorizationUrl
        self.tokenUrl = tokenUrl
        self.scopes = scopes
        self.clientIdEnv = clientIdEnv
        self.clientSecretEnv = clientSecretEnv
        self.authorizationParams = authorizationParams
    }
}

/// The Google Workspace connection entry: which grant it registers, which
/// Keychain-backed variables hold the client identity, and which services
/// become hosts once the grant is authorized.
public struct GoogleConnectionConfig: Codable {
    public var grant: String
    public var clientIdEnv: String
    /// Empty means a client without a secret: the composition is TOML, which
    /// has no null, so the empty name is the spelling the plugin reads as
    /// "none".
    public var clientSecretEnv: String
    public var services: [String]
    public var sourcesMax: Int

    enum CodingKeys: String, CodingKey {
        case grant, clientIdEnv, clientSecretEnv, services, sourcesMax
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        grant = try container.decode(String.self, forKey: .grant)
        clientIdEnv = try container.decode(String.self, forKey: .clientIdEnv)
        clientSecretEnv = try container.decodeIfPresent(String.self, forKey: .clientSecretEnv) ?? ""
        services = try container.decode([String].self, forKey: .services)
        sourcesMax = try container.decode(Int.self, forKey: .sourcesMax)
    }
}

/// The services the Google connection can steward, in catalog order; the
/// raw value is the composition's spelling.
public enum GoogleService: String, CaseIterable, Identifiable {
    case gmail
    case drive
    case calendar
    case contacts
    case tasks

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .gmail: "Gmail"
        case .drive: "Google Drive"
        case .calendar: "Google Calendar"
        case .contacts: "Google Contacts"
        case .tasks: "Google Tasks"
        }
    }
}

public struct Configurable<Config: Codable>: Codable {
    public var enabled: Bool
    public var config: Config
}

public struct FeatureToggle: Codable {
    public var enabled: Bool
}

public struct FilesystemConfig: Codable {
    /// Identity material the host id is derived from; nil uses the
    /// machine's own id. Never the id itself.
    public var machineId: String?
    public var skipHidden: Bool
    public var gitignore: Bool
    public var ignore: [String]
    /// The folders this host indexes, absolute. Empty: any folder named
    /// per run. Older nodes omit the key, so decoding defaults it.
    public var roots: [String]

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        machineId = try container.decodeIfPresent(String.self, forKey: .machineId)
        skipHidden = try container.decode(Bool.self, forKey: .skipHidden)
        gitignore = try container.decode(Bool.self, forKey: .gitignore)
        ignore = try container.decode([String].self, forKey: .ignore)
        roots = try container.decodeIfPresent([String].self, forKey: .roots) ?? []
    }
}

public struct LanguageModelConfig: Codable {
    public var baseUrl: String
    /// Empty for a keyless endpoint (a local ollama).
    public var apiKeyEnv: String
    public var transformModel: String
    /// Reasoning effort for transform calls as the endpoint spells it
    /// (`none` keeps a thinking model from answering with an empty reply).
    public var transformReasoningEffort: String?
    public var agentModel: String
}

public enum EmbedderProvider: String, Codable, CaseIterable, Identifiable {
    case endpoint
    case hashed
    case none

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .endpoint: "API endpoint"
        case .hashed: "Local hashed"
        case .none: "Full-text only"
        }
    }
}

public enum VectorScope: String, Codable, CaseIterable, Identifiable {
    case all
    case summaries

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .all: "Every fragment"
        case .summaries: "Summaries only"
        }
    }
}

public struct EmbedderConfig: Codable {
    public var provider: EmbedderProvider
    public var model: String
    /// Nil takes the model's native width.
    public var dimensions: Int?
    public var vectors: VectorScope
}

public struct ChunkerConfig: Codable {
    public var targetChars: Int
}

public struct SummarizerConfig: Codable {
    public var targetChars: Int
    public var llmCallBudget: Int
}

public struct EntityConfig: Codable {
    public var maxPerSource: Int
    public var llmCallBudget: Int
}

public struct FinderConfig: Codable {
    public var seedK: Int
    public var rrfK: Double
    public var damping: Double
    public var iterations: Int
    public var epsilon: Double
    public var maxHints: Int
    public var maxVectorDistance: Double
    public var weights: RelationWeights
}

public struct RelationWeights: Codable {
    public var `default`: Double
    public var byKind: [String: Double]
}

public struct SweepConfig: Codable {
    public var maxSources: Int
    public var concurrency: Int
    public var batchConcurrency: Int
    public var sourceReadsInFlightMax: Int
    public var maxFragmentsPerSource: Int
    public var maxDepth: Int
    public var maxContentBytes: UInt64
    public var maxReferenceHops: Int
    public var modifiedAfter: String?
    public var ignore: [IgnoreRule]
}

public struct IgnoreRule: Codable, Identifiable {
    public var id = UUID()
    public var address: String?
    public var host: String?
    public var locator: String?
    public var sourceType: String?
    public var contentType: String?
    public var hint: String?
    public var property: String?

    enum CodingKeys: String, CodingKey {
        case address, host, locator, sourceType, contentType, hint, property
    }

    /// Every field defaults to nil so a rule can be built from the one or
    /// two fields it matches on, the way the apps' "Add rule" buttons do.
    public init(
        address: String? = nil,
        host: String? = nil,
        locator: String? = nil,
        sourceType: String? = nil,
        contentType: String? = nil,
        hint: String? = nil,
        property: String? = nil
    ) {
        self.address = address
        self.host = host
        self.locator = locator
        self.sourceType = sourceType
        self.contentType = contentType
        self.hint = hint
        self.property = property
    }
}

// Codable mirrors of the plugin owner operations. JSON uses snake_case at
// the FFI boundary through CoreNode's encoder and decoder.

public struct InstallPluginRequest: Encodable {
    public let id: String
    public let files: [PluginFile]
}

public struct PluginFile: Encodable {
    public let path: String
    public let bytes: String
}

public enum PluginState: Codable, Equatable {
    case active
    case pending
    case failed(reason: String)

    private enum CodingKeys: String, CodingKey {
        case state
        case reason
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let state = try container.decode(String.self, forKey: .state)
        switch state {
        case "active": self = .active
        case "pending": self = .pending
        case "failed":
            self = .failed(reason: try container.decode(String.self, forKey: .reason))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .state,
                in: container,
                debugDescription: "unknown plugin state `\(state)`"
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .active:
            try container.encode("active", forKey: .state)
        case .pending:
            try container.encode("pending", forKey: .state)
        case .failed(let reason):
            try container.encode("failed", forKey: .state)
            try container.encode(reason, forKey: .reason)
        }
    }
}

public struct PluginView: Codable, Identifiable, Equatable {
    public let id: String
    public let plugin: String
    public let state: PluginState
    public let effects: [String]
    public let missing: [String]
    public let missingSecrets: [SecretNeed]

    public var stateLabel: String {
        switch state {
        case .active: "active"
        case .pending:
            missing.isEmpty ? "pending" : "waiting for \(missing.joined(separator: ", "))"
        case .failed(let reason): "failed: \(reason)"
        }
    }
}
