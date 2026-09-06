import Foundation
import Testing
@testable import InseamKit

/// An in-memory host: what a Photos or call-recordings bridge does, minus
/// the platform framework.
private final class TableHost: BridgedHostSource, @unchecked Sendable {
    let sources: [String: (contentType: String, data: Data)]
    let reads = Locked<[String]>([])

    init(_ entries: [(String, String, String)]) {
        var sources: [String: (contentType: String, data: Data)] = [:]
        for (locator, contentType, text) in entries {
            sources[locator] = (contentType, Data(text.utf8))
        }
        self.sources = sources
    }

    func enumerate(root: String) throws -> [BridgedSource] {
        sources.keys.sorted().filter { $0.hasPrefix(root) }.map { locator in
            let entry = sources[locator]!
            return BridgedSource(
                locator: locator,
                sourceType: "note",
                contentType: entry.contentType,
                bytes: UInt64(entry.data.count),
                title: locator,
                properties: [BridgedProperty(key: "participant", value: "+15550100")]
            )
        }
    }

    func readBytes(locator: String) throws -> Data {
        reads.mutate { $0.append(locator) }
        guard let entry = sources[locator] else {
            throw BridgeError(description: "no source `\(locator)`")
        }
        return entry.data
    }
}

/// A unit-vector embedder keyed by the text's first byte: enough to prove
/// vectors flow from Swift into the store and back out of a query.
private final class UnitEmbedder: ShellEmbedding, @unchecked Sendable {
    let model = "swift/unit"
    let dimensions: UInt32 = 8
    let calls = Locked(0)

    func embed(_ texts: [String]) throws -> [[Float]] {
        calls.mutate { $0 += 1 }
        return texts.map { text in
            var vector = [Float](repeating: 0, count: 8)
            let slot = Int(text.utf8.first ?? 0) % 8
            vector[slot] = 1
            return vector
        }
    }
}

/// A tiny lock so the fakes are honestly thread-safe under the core's
/// blocking pool.
private final class Locked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()

    init(_ value: T) {
        self.value = value
    }

    func mutate(_ body: (inout T) -> Void) {
        lock.lock()
        defer { lock.unlock() }
        body(&value)
    }

    func read() -> T {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

private func offlineDataDir() throws -> URL {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("inseamkit-bridge-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    // The shell's embedder replaces the base `embedder` entry; the llm
    // stays off so nothing parks on a missing key.
    try """
    [[entry]]
    id = "llm"
    disabled = true

    """.write(to: dir.appendingPathComponent("composition.toml"), atomically: true, encoding: .utf8)
    return dir
}

@Suite struct BridgeTests {
    @Test func aBridgedHostIsIndexedThroughTheShellEmbedderAndFound() throws {
        let dir = try offlineDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let embedder = UnitEmbedder()
        let node = try CoreNode(dataDir: dir, embedder: embedder)
        defer { node.close() }

        let health = try node.health()
        let embedderEntry = try #require(health.first { $0.id == "embedder" })
        #expect(embedderEntry.plugin == "embedder-app")
        #expect(embedderEntry.state == "active")

        let host = TableHost([
            ("calls/2026-09-01", "text/plain", "Talked about the kitchen renovation quote."),
            ("calls/2026-09-02", "text/plain", "Dentist appointment moved to Thursday."),
        ])
        let view = try node.registerHost(
            BridgedHostDescription(kind: "phone", principal: "test-iphone", displayName: "Test iPhone"),
            source: host
        )
        #expect(view.id.hasPrefix("phone-"))
        #expect(view.entry == "app:phone")
        #expect(try node.hosts().contains { $0.id == view.id })

        let report = try node.indexHost(view.id, root: "calls/")
        #expect(report.indexed == 2)
        #expect(embedder.calls.read() >= 1)
        #expect(host.reads.read().count >= 2)

        let results = try node.query("dentist appointment").results
        #expect(results.contains { $0.address.hasSuffix("calls/2026-09-02") })

        try node.unregisterHost(view.id)
        #expect(!(try node.hosts().contains { $0.id == view.id }))
        #expect(throws: CoreError.self) { try node.unregisterHost(view.id) }
    }

    @Test func aSecondRegistrationOfTheSameHostIsRefusedByName() throws {
        let dir = try offlineDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let node = try CoreNode(dataDir: dir, embedder: UnitEmbedder())
        defer { node.close() }
        let description = BridgedHostDescription(kind: "phone", principal: "twin", displayName: "Twin")
        let view = try node.registerHost(description, source: TableHost([]))
        #expect(throws: CoreError.self) {
            try node.registerHost(description, source: TableHost([]))
        }
        #expect(try node.hosts().filter { $0.id == view.id }.count == 1)
    }

    @Test func aControlledIndexCallReportsProgressAndStopsBeforeReading() throws {
        let dir = try offlineDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let node = try CoreNode(dataDir: dir, embedder: UnitEmbedder())
        defer { node.close() }
        let source = TableHost([
            ("notes/one", "text/plain", "one"),
            ("notes/two", "text/plain", "two"),
        ])
        let host = try node.registerHost(
            BridgedHostDescription(kind: "phone", principal: "controlled", displayName: "Phone"),
            source: source
        )
        let phases = Locked<[IndexPhase]>([])
        let controller = IndexController { progress in
            phases.mutate { $0.append(progress.phase) }
        }
        controller.stop()

        let report = try node.indexHost(host.id, controller: controller)

        #expect(report.stopped)
        #expect(source.reads.read().isEmpty)
        #expect(phases.read() == [.preparing, .complete])
    }
}
