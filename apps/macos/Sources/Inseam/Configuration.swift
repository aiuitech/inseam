import Foundation

struct ConfigurationSettings: Codable {
    var connections: FeatureToggle
    var fs: Configurable<FilesystemConfig>
    var oauth: Configurable<OAuthConfig>
    var llm: Configurable<LanguageModelConfig>
    var embedder: Configurable<EmbedderConfig>
    var transforms: FeatureToggle
    var markdown: FeatureToggle
    var chunker: Configurable<ChunkerConfig>
    var summarizer: Configurable<SummarizerConfig>
    var entities: Configurable<EntityConfig>
    var finder: Configurable<FinderConfig>
    var sweep: Configurable<SweepConfig>
    var operations: FeatureToggle
}

struct OAuthConfig: Codable {
    var callbackPort: Int
    var authorizationTimeoutSecs: UInt64
    var credentialsDir: String?
    var grants: [OAuthGrantConfig]
}

struct OAuthGrantConfig: Codable, Identifiable {
    var formId = UUID()
    var grantId: String
    var authorizationUrl: String
    var tokenUrl: String
    var scopes: [String]
    var clientIdEnv: String
    var clientSecretEnv: String?
    var authorizationParams: [String: String]

    enum CodingKeys: String, CodingKey {
        case grantId = "id"
        case authorizationUrl, tokenUrl, scopes, clientIdEnv, clientSecretEnv
        case authorizationParams
    }

    var id: UUID { formId }
}

struct Configurable<Config: Codable>: Codable {
    var enabled: Bool
    var config: Config
}

struct FeatureToggle: Codable {
    var enabled: Bool
}

struct FilesystemConfig: Codable {
    var hostId: String?
    var skipHidden: Bool
    var gitignore: Bool
    var ignore: [String]
}

struct LanguageModelConfig: Codable {
    var baseUrl: String
    var apiKeyEnv: String
    var transformModel: String
    var agentModel: String
}

enum EmbedderProvider: String, Codable, CaseIterable, Identifiable {
    case endpoint
    case hashed
    case none

    var id: String { rawValue }

    var label: String {
        switch self {
        case .endpoint: "API endpoint"
        case .hashed: "Local hashed"
        case .none: "Full-text only"
        }
    }
}

struct EmbedderConfig: Codable {
    var provider: EmbedderProvider
    var model: String
    var dimensions: Int
}

struct ChunkerConfig: Codable {
    var targetChars: Int
}

struct SummarizerConfig: Codable {
    var targetChars: Int
    var llmCallBudget: Int
}

struct EntityConfig: Codable {
    var maxPerSource: Int
    var llmCallBudget: Int
}

struct FinderConfig: Codable {
    var seedK: Int
    var rrfK: Double
    var damping: Double
    var iterations: Int
    var epsilon: Double
    var maxHints: Int
    var maxVectorDistance: Double
    var weights: RelationWeights
}

struct RelationWeights: Codable {
    var `default`: Double
    var byKind: [String: Double]
}

struct SweepConfig: Codable {
    var maxSources: Int
    var concurrency: Int
    var maxFragmentsPerSource: Int
    var maxDepth: Int
    var maxContentBytes: UInt64
    var modifiedAfter: String?
    var ignore: [IgnoreRule]
}

struct IgnoreRule: Codable, Identifiable {
    var id = UUID()
    var address: String?
    var host: String?
    var locator: String?
    var sourceType: String?
    var contentType: String?
    var hint: String?
    var property: String?

    enum CodingKeys: String, CodingKey {
        case address, host, locator, sourceType, contentType, hint, property
    }
}
