import CInseamFFI
import Foundation

/// An error surfaced by the Rust core across the FFI boundary.
struct CoreError: LocalizedError {
    let message: String
    var errorDescription: String? { message }

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
    private let handle: OpaquePointer

    static func coreVersion() -> String {
        guard let pointer = inseam_version() else { return "unknown" }
        defer { inseam_string_free(pointer) }
        return String(cString: pointer)
    }

    init(dataDir: URL) throws {
        var error: UnsafeMutablePointer<CChar>?
        guard let handle = inseam_node_open(dataDir.path, nil, &error) else {
            throw CoreError(taking: error)
        }
        self.handle = handle
    }

    deinit {
        inseam_node_free(handle)
    }

    func query(_ text: String, limit: UInt32 = 8) throws -> QueryResponse {
        try decode(QueryResponse.self) { error in
            inseam_node_query(handle, text, limit, &error)
        }
    }

    func indexDirectory(_ dir: URL, rebuild: Bool = false) throws -> IndexReport {
        try decode(IndexReport.self) { error in
            inseam_node_index_dir(handle, dir.path, rebuild, &error)
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

struct IndexReport: Decodable {
    let sourcesSeen: Int
    let indexed: Int
    let unchanged: Int
    let removed: Int
    let fragments: Int
}
