import InseamKit
import SwiftUI

/// The button. While the owner is on a call they tap *capture this call*:
/// the hosted node dials this phone, the owner answers the second call and
/// taps *merge*, and from then on the line records. The first time, the
/// owner verifies this phone's number here — the node speaks a code, they
/// type it back — so a stolen owner token cannot make the node ring
/// strangers (design/call-capture.md).
struct CallCaptureView: View {
    @Environment(NodeModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var status: CaptureStatus?
    @State private var number = ""
    @State private var code = ""
    @State private var message = ""
    @State private var working = false
    /// The owner asked to replace a verified or pending number: show the
    /// entry field again. The node keeps the old number until the new
    /// verification call is placed.
    @State private var changingNumber = false

    var body: some View {
        NavigationStack {
            List {
                if let status {
                    Section("capture line") {
                        LabeledContent("number", value: status.captureNumber)
                        LabeledContent("recordings", value: String(status.recordingsArchived))
                        if let call = status.lastCall {
                            LabeledContent("last call", value: Self.relative(call.startedAt))
                        }
                    }
                    ownerSection(status)
                    if case .verified = status.ownerNumber {
                        Section {
                            Button {
                                start()
                            } label: {
                                Label("capture this call", systemImage: "phone.badge.plus")
                                    .font(Brand.font(.headline))
                                    .frame(maxWidth: .infinity)
                            }
                            .buttonStyle(.borderedProminent)
                            .tint(Brand.thread)
                            .foregroundStyle(Brand.ground)
                            .disabled(working)
                        } footer: {
                            Text("Your phone rings with a second call. Answer it, then tap merge calls. Everything after the merge is recorded and lands in your index. Audio before the merge is not captured.")
                                .font(Brand.font(.caption2))
                        }
                    }
                } else if model.hostedNode == nil {
                    ContentUnavailableView(
                        "no hosted node",
                        systemImage: "antenna.radiowaves.left.and.right.slash",
                        description: Text("Add your hosted node's URL and owner token under settings › hosted node.")
                    )
                } else if message.isEmpty {
                    ProgressView().tint(Brand.thread)
                }
                if !message.isEmpty {
                    Text(message).font(Brand.font(.caption)).foregroundStyle(.secondary)
                }
            }
            .navigationTitle("call capture")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("done") { dismiss() }
                }
            }
            .task { await refresh() }
        }
        .preferredColorScheme(.dark)
        .font(Brand.font(.body))
    }

    @ViewBuilder
    private func ownerSection(_ status: CaptureStatus) -> some View {
        switch changingNumber ? CaptureStatus.OwnerNumber.unset : status.ownerNumber {
        case .unset:
            Section("your phone") {
                TextField("+1 415 555 0123", text: $number)
                    .keyboardType(.phonePad)
                    .textContentType(.telephoneNumber)
                Button("call me with a code") { setNumber() }
                    .disabled(working || number.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        case .pending(let pending, _):
            Section("your phone") {
                LabeledContent("verifying", value: Self.masked(pending))
                TextField("6-digit code", text: $code)
                    .keyboardType(.numberPad)
                    .textContentType(.oneTimeCode)
                Button("verify") { verify() }
                    .disabled(working || code.count < 6)
                Button("use a different number") { reset() }
                    .disabled(working)
            }
        case .verified(let verified):
            Section("your phone") {
                LabeledContent("verified", value: Self.masked(verified))
                Button("use a different number") { reset() }
                    .disabled(working)
            }
        }
    }

    private func refresh() async {
        guard let client = model.hostedNode else { return }
        working = true
        defer { working = false }
        do {
            status = try await client.captureStatus()
            message = ""
        } catch {
            message = "could not reach the hosted node: \(error.localizedDescription)"
        }
    }

    private func setNumber() {
        perform("verification call") { client in try await client.setCaptureNumber(number) }
    }

    private func verify() {
        perform("verification") { client in try await client.verifyCaptureNumber(code) }
    }

    private func start() {
        perform("capture call") { client in try await client.startCallCapture() }
    }

    private func reset() {
        number = ""
        code = ""
        changingNumber = true
    }

    private func perform(_ label: String, _ work: @escaping (HostedNodeClient) async throws -> CaptureStatus) {
        guard let client = model.hostedNode else { return }
        working = true
        Task {
            defer { working = false }
            do {
                status = try await work(client)
                code = ""
                changingNumber = false
                message = "\(label) placed"
            } catch {
                message = "\(label) failed: \(error.localizedDescription)"
            }
        }
    }

    private static func masked(_ number: String) -> String {
        let digits = number.dropFirst()
        let keep = min(4, digits.count)
        return "+" + String(repeating: "•", count: digits.count - keep) + digits.suffix(keep)
    }

    private static func relative(_ epochSeconds: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(epochSeconds))
        return RelativeDateTimeFormatter().localizedString(for: date, relativeTo: Date())
    }
}
