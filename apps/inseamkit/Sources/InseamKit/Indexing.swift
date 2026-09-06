import CInseamFFI
import Foundation

/// The stable phases a native app can present for one indexing run.
public enum IndexPhase: String, Decodable, Sendable {
    case preparing
    case enumerating
    case cataloging
    case indexing
    case finalizing
    case complete

    public var label: String {
        switch self {
        case .preparing: "Preparing"
        case .enumerating: "Finding sources"
        case .cataloging: "Updating catalog"
        case .indexing: "Indexing"
        case .finalizing: "Finishing search index"
        case .complete: "Complete"
        }
    }
}

/// One bounded progress snapshot from the core.
public struct IndexProgress: Decodable, Equatable, Sendable {
    public let phase: IndexPhase
    public let sourcesComplete: Int
    public let sourcesTotal: Int
    public let current: String?
    public let indexed: Int
    public let unchanged: Int
    public let catalogOnly: Int
    public let ignored: Int
    public let stopped: Bool

    public var fractionCompleted: Double? {
        guard sourcesTotal > 0 else { return nil }
        return min(Double(sourcesComplete) / Double(sourcesTotal), 1)
    }
}

/// Thread-safe command state retained by one blocking index call. The core
/// polls while paused, so resume and stop take effect without cancelling the
/// Swift task that owns the FFI call.
public final class IndexController: @unchecked Sendable {
    private enum Command: UInt32 {
        case run = 0
        case pause = 1
        case stop = 2
    }

    private let lock = NSLock()
    private let onProgress: @Sendable (IndexProgress) -> Void
    private var command = Command.run
    private var lastProgress: IndexProgress?

    public init(onProgress: @escaping @Sendable (IndexProgress) -> Void) {
        self.onProgress = onProgress
    }

    public func pause() {
        setCommand(.pause)
    }

    public func resume() {
        setCommand(.run)
    }

    public func stop() {
        setCommand(.stop)
    }

    func receive(progressJSON: String) -> UInt32 {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let progress = try? decoder.decode(IndexProgress.self, from: Data(progressJSON.utf8)) else {
            return Command.stop.rawValue
        }
        lock.lock()
        let changed = progress != lastProgress
        lastProgress = progress
        let current = command
        lock.unlock()
        if changed {
            onProgress(progress)
        }
        return current.rawValue
    }

    private func setCommand(_ next: Command) {
        lock.lock()
        command = next
        lock.unlock()
    }
}

private func indexUpdate(
    userData: UnsafeMutableRawPointer?,
    progressJSON: UnsafePointer<CChar>?
) -> UInt32 {
    guard let userData, let progressJSON else { return 2 }
    let controller = Unmanaged<IndexController>.fromOpaque(userData).takeUnretainedValue()
    return controller.receive(progressJSON: String(cString: progressJSON))
}

extension CoreNode {
    /// Index a directory with live progress and cooperative pause or stop.
    public func indexDirectory(
        _ dir: URL,
        rebuild: Bool = false,
        controller: IndexController
    ) throws -> IndexReport {
        try withHandle { handle in
            var callbacks = InseamIndexCallbacks(update: indexUpdate)
            let context = Unmanaged.passUnretained(controller).toOpaque()
            return try decode(IndexReport.self) { error in
                inseam_node_index_dir_controlled(
                    handle, dir.path, rebuild, &callbacks, context, &error
                )
            }
        }
    }

    /// Index one host scope with live progress and cooperative pause or stop.
    public func indexHost(
        _ hostId: String,
        root: String = "",
        rebuild: Bool = false,
        controller: IndexController
    ) throws -> IndexReport {
        try withHandle { handle in
            var callbacks = InseamIndexCallbacks(update: indexUpdate)
            let context = Unmanaged.passUnretained(controller).toOpaque()
            return try decode(IndexReport.self) { error in
                inseam_node_index_host_controlled(
                    handle, hostId, root, rebuild, &callbacks, context, &error
                )
            }
        }
    }
}
