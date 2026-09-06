import CInseamFFI
import Foundation

// The app shell as a connection and as a provider (`design/ios-app.md`).
// A host only the shell's process can reach — the Photos library, a
// folder behind a security-scoped bookmark — is bridged into the node as
// an ordinary host: the shell enumerates and reads, the node catalogs,
// indexes, and serves. An on-device model the shell can run — a sentence
// embedder — is bridged in as the node's `embedder` provider. Both cross
// the C boundary as callbacks; this file is the Swift side of that
// contract, so app code implements two small protocols and never touches
// a pointer.
//
// Threading: the core calls these from its blocking pool, on any thread,
// possibly several at once. Implementations must be safe to call that way
// (`Sendable`), and must not block on the main actor — the main actor may
// itself be waiting on the core.

/// One source as the shell enumerates it. Mirrors the core's
/// `BridgedSource`; length is bytes, timestamps are Unix seconds.
public struct BridgedSource: Encodable, Equatable, Sendable {
    public var locator: String
    public var sourceType: String
    public var contentType: String
    public var bytes: UInt64
    public var created: Int64?
    public var modified: Int64?
    public var title: String?
    public var properties: [BridgedProperty]

    public init(
        locator: String,
        sourceType: String,
        contentType: String,
        bytes: UInt64,
        created: Int64? = nil,
        modified: Int64? = nil,
        title: String? = nil,
        properties: [BridgedProperty] = []
    ) {
        self.locator = locator
        self.sourceType = sourceType
        self.contentType = contentType
        self.bytes = bytes
        self.created = created
        self.modified = modified
        self.title = title
        self.properties = properties
    }
}

/// A `key:value` trust property on a source's envelope, claimed by the
/// shell (a call's participant, a photo's place).
public struct BridgedProperty: Encodable, Equatable, Sendable {
    public var key: String
    public var value: String

    public init(key: String, value: String) {
        self.key = key
        self.value = value
    }
}

/// How the shell describes a host it bridges. The host id is derived by
/// the core from `kind` and `principal`.
public struct BridgedHostDescription: Encodable, Equatable, Sendable {
    public var kind: String
    /// The host's own identity — a device or account — never the app.
    public var principal: String
    public var displayName: String

    public init(kind: String, principal: String, displayName: String) {
        self.kind = kind
        self.principal = principal
        self.displayName = displayName
    }
}

/// What a bridged host does: list what is under a root and read one
/// source's bytes. Locators are the shell's flat id space; a root is a
/// prefix over it (`""` is everything).
public protocol BridgedHostSource: AnyObject, Sendable {
    func enumerate(root: String) throws -> [BridgedSource]
    func readBytes(locator: String) throws -> Data
}

/// An on-device embedding model the shell runs for the node. `dimensions`
/// is the width every vector has; the identity (`model` + width) is what
/// the store binds the search surface to, so change it and the node
/// re-embeds.
public protocol ShellEmbedding: AnyObject, Sendable {
    var model: String { get }
    var dimensions: UInt32 { get }
    /// Vectors in input order, each exactly `dimensions` wide.
    func embed(_ texts: [String]) throws -> [[Float]]
}

/// Most texts the core hands one `embed` call (mirrors the core's ceiling).
public let shellEmbedTextsMax = 1_024

/// The identity the shell declares for its embedder, as the core reads it.
struct ShellEmbedderDescription: Encodable {
    let model: String
    let dimensions: UInt32
}

// MARK: - C callback plumbing

/// The retained context handed to the core as `user_data`: one Swift
/// object behind an opaque pointer, released exactly once when the core
/// says it is done (the `release` callback).
private final class HostContext {
    let source: BridgedHostSource

    init(source: BridgedHostSource) {
        self.source = source
    }
}

private final class EmbedderContext {
    let embedder: ShellEmbedding

    init(embedder: ShellEmbedding) {
        self.embedder = embedder
    }
}

/// Copy a Swift string into a C string the core frees through our
/// `free_string` — which is plain `free`.
private func cString(_ string: String) -> UnsafeMutablePointer<CChar> {
    strdup(string)
}

private func setError(_ slot: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?, _ error: Error) {
    guard let slot else { return }
    slot.pointee = cString(String(describing: error))
}

private func encodeJSON<T: Encodable>(_ value: T) throws -> String {
    let encoder = JSONEncoder()
    encoder.keyEncodingStrategy = .convertToSnakeCase
    encoder.outputFormatting = [.sortedKeys]
    return String(decoding: try encoder.encode(value), as: UTF8.self)
}

/// A failure inside a bridged callback, with the message the core shows.
struct BridgeError: Error, CustomStringConvertible {
    let description: String
}

/// The C callbacks over a `BridgedHostSource`. Every buffer these return is
/// malloc'd, so the matching free callbacks are `free`.
enum HostBridge {
    static func callbacks() -> InseamHostCallbacks {
        InseamHostCallbacks(
            enumerate: { userData, root, errorOut in
                guard let userData, let root else { return nil }
                let context = Unmanaged<HostContext>.fromOpaque(userData).takeUnretainedValue()
                do {
                    let sources = try context.source.enumerate(root: String(cString: root))
                    return cString(try encodeJSON(sources))
                } catch {
                    setError(errorOut, error)
                    return nil
                }
            },
            read_bytes: { userData, locator, lengthOut, errorOut in
                guard let userData, let locator, let lengthOut else { return nil }
                let context = Unmanaged<HostContext>.fromOpaque(userData).takeUnretainedValue()
                do {
                    let data = try context.source.readBytes(locator: String(cString: locator))
                    return HostBridge.copyOut(data, lengthOut: lengthOut)
                } catch {
                    setError(errorOut, error)
                    return nil
                }
            },
            free_string: { _, string in free(string) },
            free_bytes: { _, bytes, _ in free(bytes) },
            release: { userData in
                guard let userData else { return }
                Unmanaged<HostContext>.fromOpaque(userData).release()
            }
        )
    }

    static func retain(_ source: BridgedHostSource) -> UnsafeMutableRawPointer {
        Unmanaged.passRetained(HostContext(source: source)).toOpaque()
    }

    /// Give a refused registration's context back (the core never took it).
    static func releaseRefused(_ userData: UnsafeMutableRawPointer) {
        Unmanaged<HostContext>.fromOpaque(userData).release()
    }

    /// malloc a copy of `data` for the core; an empty read is still a
    /// non-null buffer so it is not mistaken for failure.
    private static func copyOut(
        _ data: Data,
        lengthOut: UnsafeMutablePointer<UInt64>
    ) -> UnsafeMutablePointer<UInt8>? {
        let count = data.count
        guard let buffer = malloc(Swift.max(count, 1)) else { return nil }
        let bytes = buffer.assumingMemoryBound(to: UInt8.self)
        data.copyBytes(to: bytes, count: count)
        lengthOut.pointee = UInt64(count)
        return bytes
    }
}

/// The C callbacks over a `ShellEmbedding`.
enum EmbedderBridge {
    static func callbacks() -> InseamEmbedderCallbacks {
        InseamEmbedderCallbacks(
            embed: { userData, textsJSON, count, errorOut in
                guard let userData, let textsJSON else { return nil }
                let context = Unmanaged<EmbedderContext>.fromOpaque(userData).takeUnretainedValue()
                do {
                    let data = Data(String(cString: textsJSON).utf8)
                    let texts = try JSONDecoder().decode([String].self, from: data)
                    guard texts.count == Int(count) else {
                        throw BridgeError(description: "embed count \(count) does not match \(texts.count) texts")
                    }
                    let vectors = try context.embedder.embed(texts)
                    return try EmbedderBridge.copyOut(vectors, width: Int(context.embedder.dimensions), count: texts.count)
                } catch {
                    setError(errorOut, error)
                    return nil
                }
            },
            free_string: { _, string in free(string) },
            free_floats: { _, floats, _ in free(floats) },
            release: { userData in
                guard let userData else { return }
                Unmanaged<EmbedderContext>.fromOpaque(userData).release()
            }
        )
    }

    static func retain(_ embedder: ShellEmbedding) -> UnsafeMutableRawPointer {
        Unmanaged.passRetained(EmbedderContext(embedder: embedder)).toOpaque()
    }

    static func releaseRefused(_ userData: UnsafeMutableRawPointer) {
        Unmanaged<EmbedderContext>.fromOpaque(userData).release()
    }

    /// One row-major malloc'd buffer of `count × width` floats. A vector of
    /// the wrong width is a programming error in the embedder, reported
    /// rather than silently padded.
    private static func copyOut(
        _ vectors: [[Float]],
        width: Int,
        count: Int
    ) throws -> UnsafeMutablePointer<Float>? {
        guard vectors.count == count else {
            throw BridgeError(description: "embedder returned \(vectors.count) vectors for \(count) texts")
        }
        let total = count * width
        guard let buffer = malloc(Swift.max(total, 1) * MemoryLayout<Float>.stride) else { return nil }
        let floats = buffer.assumingMemoryBound(to: Float.self)
        var offset = 0
        for vector in vectors {
            guard vector.count == width else {
                free(buffer)
                throw BridgeError(description: "embedder returned a \(vector.count)-wide vector; declared \(width)")
            }
            vector.withUnsafeBufferPointer { source in
                (floats + offset).update(from: source.baseAddress!, count: width)
            }
            offset += width
        }
        assert(offset == total)
        return floats
    }
}

// MARK: - CoreNode surface

extension CoreNode {
    /// Open the node with the shell's own embedder mounted, so a device
    /// with an on-device model needs no API key for vectors. The node
    /// keeps `embedder` alive until `close()`.
    public convenience init(dataDir: URL, embedder: ShellEmbedding) throws {
        let json = try encodeJSON(ShellEmbedderDescription(model: embedder.model, dimensions: embedder.dimensions))
        var callbacks = EmbedderBridge.callbacks()
        let userData = EmbedderBridge.retain(embedder)
        var error: UnsafeMutablePointer<CChar>?
        let handle = inseam_node_open_with_shell(dataDir.path, nil, json, &callbacks, userData, &error)
        guard let handle else {
            EmbedderBridge.releaseRefused(userData)
            throw CoreError(taking: error)
        }
        self.init(handle: handle)
    }

    /// Bridge a host into the node: it is listed by `hosts()` at once and
    /// indexed by `indexHost`. The node retains `source` until the host is
    /// unregistered or the node closes, and drops it only after every
    /// in-flight read has finished.
    public func registerHost(
        _ description: BridgedHostDescription,
        source: BridgedHostSource
    ) throws -> HostView {
        let json = try encodeJSON(description)
        var callbacks = HostBridge.callbacks()
        let userData = HostBridge.retain(source)
        do {
            return try withHandle { handle in
                try decode(HostView.self) { error in
                    inseam_node_register_host(handle, json, &callbacks, userData, &error)
                }
            }
        } catch {
            HostBridge.releaseRefused(userData)
            throw error
        }
    }

    /// Withdraw a bridged host. Only hosts this shell registered qualify.
    public func unregisterHost(_ hostId: String) throws {
        try withHandle { handle in
            var error: UnsafeMutablePointer<CChar>?
            guard inseam_node_unregister_host(handle, hostId, &error) else {
                throw CoreError(taking: error)
            }
        }
    }

    /// Index one scope of a stewarded host (`root` is the host's locator
    /// prefix; `""` for all of it).
    public func indexHost(_ hostId: String, root: String = "", rebuild: Bool = false) throws -> IndexReport {
        try withHandle { handle in
            try decode(IndexReport.self) { error in
                inseam_node_index_host(handle, hostId, root, rebuild, &error)
            }
        }
    }
}
