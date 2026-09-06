import SwiftUI

/// One finder result as a list row: title (or address), score, address,
/// summary when the node has one, and the content type and length line.
/// Shared by every app shell so results read the same everywhere.
public struct QueryResultRow: View {
    public let result: QueryResult

    public init(result: QueryResult) {
        self.result = result
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                Text(result.envelope.title ?? result.address)
                    .font(.headline)
                    .lineLimit(1)
                Spacer()
                Text(String(format: "%.3f", result.score))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            Text(result.address)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            if let summary = result.summary {
                Text(summary).font(.callout).lineLimit(3)
            }
            Text("\(result.envelope.contentType) · \(result.envelope.length)")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
        .padding(.vertical, 2)
    }
}
