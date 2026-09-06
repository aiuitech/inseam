import CInseamFFI
import Foundation

/// An error surfaced by the Rust core across the FFI boundary.
public struct CoreError: LocalizedError {
    public let message: String
    public var errorDescription: String? { message }

    public init(message: String) {
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
public final class CoreNode {
    private var handle: OpaquePointer?

    public static func coreVersion() -> String {
        guard let pointer = inseam_version() else { return "unknown" }
        defer { inseam_string_free(pointer) }
        return String(cString: pointer)
    }

    public static func readSettings(at compositionURL: URL) throws -> ConfigurationSettings {
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

    public static func writeSettings(
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

    public init(dataDir: URL) throws {
        var error: UnsafeMutablePointer<CChar>?
        guard let handle = inseam_node_open(dataDir.path, nil, &error) else {
            throw CoreError(taking: error)
        }
        self.handle = handle
    }

    /// Wrap a handle another opener produced (`Bridges.swift` opens with
    /// the shell's embedder).
    init(handle: OpaquePointer) {
        self.handle = handle
    }

    /// Run one FFI call against the live handle, or fail closed.
    func withHandle<T>(_ body: (OpaquePointer) throws -> T) throws -> T {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try body(handle)
    }

    /// Free the node — the kernel unwinds every fiber. Idempotent, and it
    /// blocks on kernel shutdown, so call it off the main thread. Closing
    /// explicitly lets a reopen release the data dir before the next boot.
    public func close() {
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
    public func health() throws -> [FiberHealth] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([FiberHealth].self) { error in
            inseam_node_health(handle, &error)
        }
    }

    public func query(_ text: String, limit: UInt32 = 8) throws -> QueryResponse {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(QueryResponse.self) { error in
            inseam_node_query(handle, text, limit, &error)
        }
    }

    public func indexDirectory(_ dir: URL, rebuild: Bool = false) throws -> IndexReport {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(IndexReport.self) { error in
            inseam_node_index_dir(handle, dir.path, rebuild, &error)
        }
    }

    /// The hosts this node stewards — the filesystem, and each Google
    /// service once its grant is authorized.
    public func hosts() throws -> [HostView] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([HostView].self) { error in
            inseam_node_hosts(handle, &error)
        }
    }

    /// The OAuth grants the node holds and where each stands.
    public func grants() throws -> [GrantView] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([GrantView].self) { error in
            inseam_node_grants(handle, &error)
        }
    }

    /// Every composition entry as the running kernel sees it.
    public func plugins() throws -> [PluginView] {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode([PluginView].self) { error in
            inseam_node_plugins(handle, &error)
        }
    }

    /// Read and mount one plugin directory into this open node. Directory
    /// parsing and JSON encoding are bounded but blocking, so call off the
    /// main thread along with the FFI call.
    public func installPlugin(id: String, directory: URL) throws -> PluginView {
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
    public static func inspectPluginDirectory(_ directory: URL) throws -> PluginDirectoryInspection {
        try PluginUpload.inspect(directory: directory)
    }

    public static func pluginIdProblem(_ id: String) -> String? {
        PluginUpload.pluginIdProblem(id)
    }

    /// Start authorizing a grant over the loopback redirect; the caller
    /// opens `url` in the browser, then blocks on `authorizeAwait`.
    public func authorizeBegin(grant: String) throws -> AuthorizationStarted {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(AuthorizationStarted.self) { error in
            inseam_node_authorize_begin(handle, grant, &error)
        }
    }

    /// Wait for the browser to come back — blocks for up to the oauth
    /// entry's timeout, so call it off the main thread.
    public func authorizeAwait(state: String) throws -> GrantView {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(GrantView.self) { error in
            inseam_node_authorize_await(handle, state, &error)
        }
    }

    /// Forget a grant's tokens; its hosts withdraw.
    public func revokeGrant(_ grant: String) throws -> GrantView {
        guard let handle else { throw CoreError(message: "node is closed") }
        return try decode(GrantView.self) { error in
            inseam_node_revoke_grant(handle, grant, &error)
        }
    }

    func decode<T: Decodable>(
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

public struct QueryResponse: Decodable {
    public let results: [QueryResult]
}

public struct QueryResult: Decodable, Identifiable {
    public let address: String
    public let score: Double
    public let summary: String?
    public let envelope: EnvelopeView

    public var id: String { address }
}

public struct EnvelopeView: Decodable {
    public let sourceType: String
    public let contentType: String
    public let length: ContentLengthView
    public let created: String?
    public let modified: String?
    public let title: String?
}

/// A source's recorded length with its unit — `lines` for text the index
/// has read (the bound a `scan` range can reach), else `bytes`.
public struct ContentLengthView: Decodable, CustomStringConvertible {
    public let unit: String
    public let value: UInt64

    public var description: String { "\(value) \(unit)" }
}

public struct FiberHealth: Decodable, Identifiable {
    public let id: String
    public let plugin: String
    public let state: String
    public let error: String?
    public let missing: [String]
    public let missingSecrets: [SecretNeed]
}

/// A secret a parked entry declared: the environment variable to set and
/// the owner-facing reason to set it — prose the UI shows verbatim.
public struct SecretNeed: Codable, Identifiable, Equatable {
    public let env: String
    public let purpose: String

    public var id: String { env }
}

public struct HostView: Decodable, Identifiable {
    public let id: String
    public let kind: String
    public let displayName: String
    public let entry: String
}

/// One OAuth grant as the node reports it (`GrantView`).
public struct GrantView: Decodable, Identifiable {
    public let id: String
    public let provider: String
    public let scopes: [String]
    public let clientIdEnv: String
    public let clientSecretEnv: String?
    public let state: GrantStateView
}

/// The tagged `GrantState`: `missing_secret` names the variable, `authorized`
/// carries the account and expiry.
public struct GrantStateView: Decodable {
    public let state: String
    public let env: String?
    public let account: String?
    public let expiresAt: Int64?
    public let scopes: [String]?

    public var isAuthorized: Bool { state == "authorized" }
    public var isMissingSecret: Bool { state == "missing_secret" }
}

public struct AuthorizationStarted: Decodable {
    public let grant: String
    public let url: String
    public let state: String
    public let redirectUri: String
}

public struct IndexReport: Decodable {
    public let sourcesSeen: Int
    public let indexed: Int
    public let unchanged: Int
    public let removed: Int
    public let fragments: Int
}
