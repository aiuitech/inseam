import Foundation
import InseamKit
import NaturalLanguage

/// Apple's on-device sentence embedder (`NLContextualEmbedding`, iOS 17+)
/// as the node's `embedder` provider: 512-dimension vectors from a
/// BERT-style model that Apple ships and downloads on demand, so the node
/// needs no API key for vectors (design/ios-app.md). One model per script
/// family; this shell picks the Latin-script model.
///
/// The framework object is not documented as thread-safe and the core
/// calls from several threads, so calls are serialized behind a lock.
/// Token vectors are mean-pooled into one sentence vector.
final class AppleEmbedder: ShellEmbedding, @unchecked Sendable {
    /// The identity the store binds to; bump the suffix if pooling changes,
    /// since that changes every vector.
    let model = "apple/nlcontextualembedding-latin-mean1"
    let dimensions: UInt32
    private let embedding: NLContextualEmbedding
    private let lock = NSLock()

    /// Most characters a text keeps; the model's sequence is short (256
    /// tokens) and the sweep chunks upstream.
    static let charactersMax = 2_000

    private init(embedding: NLContextualEmbedding) {
        self.embedding = embedding
        dimensions = UInt32(embedding.dimension)
        assert(dimensions > 0)
    }

    /// The embedder when the device has the model's assets, else nil (the
    /// node then opens with the base embedder, which needs an endpoint).
    /// Assets that are missing are requested so the next open has them.
    static func ifAvailable() -> AppleEmbedder? {
        guard let embedding = NLContextualEmbedding(script: .latin) else { return nil }
        guard embedding.hasAvailableAssets else {
            embedding.requestAssets { _, _ in }
            return nil
        }
        do {
            try embedding.load()
        } catch {
            return nil
        }
        return AppleEmbedder(embedding: embedding)
    }

    func embed(_ texts: [String]) throws -> [[Float]] {
        assert(texts.count <= shellEmbedTextsMax)
        lock.lock()
        defer { lock.unlock() }
        return try texts.map { try embedOne(String($0.prefix(Self.charactersMax))) }
    }

    private func embedOne(_ text: String) throws -> [Float] {
        let width = Int(dimensions)
        var pooled = [Float](repeating: 0, count: width)
        guard !text.isEmpty else { return pooled }
        let result = try embedding.embeddingResult(for: text, language: .english)
        var tokens = 0
        result.enumerateTokenVectors(in: text.startIndex..<text.endIndex) { vector, _ in
            assert(vector.count == width)
            for (index, value) in vector.enumerated() {
                pooled[index] += Float(value)
            }
            tokens += 1
            return true
        }
        if tokens > 0 {
            let scale = 1 / Float(tokens)
            for index in pooled.indices {
                pooled[index] *= scale
            }
        }
        return pooled
    }
}
