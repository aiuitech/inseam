import CInseamFFI
import Foundation

/// An error surfaced by the Rust core across the FFI boundary.
struct CoreError: LocalizedError {
    let message: String
    var errorDescription: String? { message }

    init(message: String) {
        self.message = message
    }

    /// Consume and free an error string the core allocated.
    init(taking pointer: UnsafeMutablePointer<CChar>?) {
        if let pointer {
            message = String(cString: pointer)
            inseam_string_free(pointer)
        } else {
            message = "unknown core error"
        }
    }
}

/// Swift face of one open node: owns the FFI handle for its lifetime.
/// FFI calls block, so callers run them off the main thread.
final class CoreNode {
    private var handle: OpaquePointer?

    static func coreVersion() -> String {
        guard let pointer = inseam_version() else { return "unknown" }
        defer { inseam_string_free(pointer) }
        return String(cString: pointer)
    }

    static func readSettings(at compositionURL: URL) throws -> ConfigurationSettings {
        var error: UnsafeMutablePointer<CChar>?
        guard let json = inseam_settings_read(compositionURL.path, &error) else {
            throw CoreError(taking: error)
        }
        defer { inseam_string_free(json) }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(
            ConfigurationSettings.self,
            from: Data(String(cString: json).utf8)
        )
    }

    static func writeSettings(
        _ settings: ConfigurationSettings,
        to compositionURL: URL
    ) throws {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        encoder.outputFormatting = [.sortedKeys]
        let json = try String(decoding: encoder.encode(settings), as: UTF8.self)
        var error: UnsafeMutablePointer<CChar>?
        guard inseam_settings_write(compositionURL.path, json, &error) else {
            throw CoreError(taking: error)
        }
    }

    init(dataDir: URL) throws {
        var error: UnsafeMutablePointer<CChar>?
        guard let handle = inseam_node_open(dataDir.path, nil, &error) else {
            throw CoreError(taking: error)
        }
        self.handle = handle
    }

    /// Free the node — the kernel unwinds every fiber. Idempotent, and it
    /// blocks on kernel shutdown, so call it off the main thread. Closing
    /// explicitly lets a reopen release the data dir before the next boot.
    func close() {
        if let handle {
            inseam_node_free(handle)
        }
        handle = nil
    }

    deinit {
        close()
    }

    /// Per-entry fiber health: which composition entries are active, and
    /// which are parked (failed, or pending on a failed one's services).
    func health() throws -> [FiberHealth] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([FiberHealth].self) { error in
            inseam_node_health(handle, &error)
        }
    }

    func query(_ text: String, limit: UInt32 = 8) throws -> QueryResponse {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(QueryResponse.self) { error in
            inseam_node_query(handle, text, limit, &error)
        }
    }

    func indexDirectory(_ dir: URL, rebuild: Bool = false) throws -> IndexReport {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(IndexReport.self) { error in
            inseam_node_index_dir(handle, dir.path, rebuild, &error)
        }
    }

    /// The hosts this node stewards — the filesystem, and each Google
    /// service once its grant is authorized.
    func hosts() throws -> [HostView] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([HostView].self) { error in
            inseam_node_hosts(handle, &error)
        }
    }

    /// The OAuth grants the node holds and where each stands.
    func grants() throws -> [GrantView] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([GrantView].self) { error in
            inseam_node_grants(handle, &error)
        }
    }

    /// Every composition entry as the running kernel sees it.
    func plugins() throws -> [PluginView] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([PluginView].self) { error in
            inseam_node_plugins(handle, &error)
        }
    }

    /// Read and mount one plugin directory into this open node. Directory
    /// parsing and JSON encoding are bounded but blocking, so call off the
    /// main thread along with the FFI call.
    func installPlugin(id: String, directory: URL) throws -> PluginView {
        guard let handle else { throw CoreError(message: "node is closed") }
        let request = try PluginUpload.request(id: id, directory: directory)
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        encoder.outputFormatting = [.sortedKeys]
        let json = try String(decoding: encoder.encode(request), as: UTF8.self)
        return try decode(PluginView.self) { error in
            inseam_node_install_plugin(handle, json, &error)
        }
    }

    /// Inspect a chosen directory before presenting the install sheet.
    static func inspectPluginDirectory(_ directory: URL) throws -> PluginDirectoryInspection {
        try PluginUpload.inspect(directory: directory)
    }

    static func pluginIdProblem(_ id: String) -> String? {
        PluginUpload.pluginIdProblem(id)
    }

    /// Start authorizing a grant over the loopback redirect; the caller
    /// opens `url` in the browser, then blocks on `authorizeAwait`.
    func authorizeBegin(grant: String) throws -> AuthorizationStarted {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(AuthorizationStarted.self) { error in
            inseam_node_authorize_begin(handle, grant, &error)
        }
    }

    /// Wait for the browser to come back — blocks for up to the oauth
    /// entry's timeout, so call it off the main thread.
    func authorizeAwait(state: String) throws -> GrantView {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(GrantView.self) { error in
            inseam_node_authorize_await(handle, state, &error)
        }
    }

    /// Forget a grant's tokens; its hosts withdraw.
    func revokeGrant(_ grant: String) throws -> GrantView {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(GrantView.self) { error in
            inseam_node_revoke_grant(handle, grant, &error)
        }
    }

    private func decode<T: Decodable>(
        _ type: T.Type,
        _ call: (inout UnsafeMutablePointer<CChar>?) -> UnsafeMutablePointer<CChar>?
    ) throws -> T {
        var error: UnsafeMutablePointer<CChar>?
        guard let json = call(&error) else { throw CoreError(taking: error) }
        defer { inseam_string_free(json) }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(type, from: Data(String(cString: json).utf8))
    }
}

// JSON views mirroring the core's `ops` responses (snake_case in transit).

struct QueryResponse: Decodable {
    let results: [QueryResult]
}

struct QueryResult: Decodable, Identifiable {
    let address: String
    let score: Double
    let summary: String?
    let envelope: EnvelopeView

    var id: String { address }
}

struct EnvelopeView: Decodable {
    let sourceType: String
    let contentType: String
    let length: String
    let created: String?
    let modified: String?
    let title: String?
}

struct FiberHealth: Decodable, Identifiable {
    let id: String
    let plugin: String
    let state: String
    let error: String?
    let missing: [String]
    let missingSecrets: [SecretNeed]
}

/// A secret a parked entry declared: the environment variable to set and
/// the owner-facing reason to set it — prose the UI shows verbatim.
struct SecretNeed: Codable, Identifiable, Equatable {
    let env: String
    let purpose: String

    var id: String { env }
}

struct HostView: Decodable, Identifiable {
    let id: String
    let kind: String
    let displayName: String
    let entry: String
}

/// One OAuth grant as the node reports it (`GrantView`).
struct GrantView: Decodable, Identifiable {
    let id: String
    let provider: String
    let scopes: [String]
    let clientIdEnv: String
    let clientSecretEnv: String?
    let state: GrantStateView
}

/// The tagged `GrantState`: `missing_secret` names the variable, `authorized`
/// carries the account and expiry.
struct GrantStateView: Decodable {
    let state: String
    let env: String?
    let account: String?
    let expiresAt: Int64?
    let scopes: [String]?

    var isAuthorized: Bool { state == "authorized" }
    var isMissingSecret: Bool { state == "missing_secret" }
}

struct AuthorizationStarted: Decodable {
    let grant: String
    let url: String
    let state: String
    let redirectUri: String
}

struct IndexReport: Decodable {
    let sourcesSeen: Int
    let indexed: Int
    let unchanged: Int
    let removed: Int
    let fragments: Int
}
