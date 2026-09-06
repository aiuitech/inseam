import InseamKit
import SwiftUI

/// The client: a search field, results, and the two things this device
/// stewards. Brand: dark ground, chartreuse thread, monospace everywhere.
struct ContentView: View {
    @Environment(NodeModel.self) private var model
    @State private var queryText = ""

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                StitchStrip(height: 10)
                    .foregroundStyle(Brand.thread)
                TextField("search your data…", text: $queryText)
                    .textFieldStyle(.roundedBorder)
                    .font(Brand.font(.body))
                    .autocorrectionDisabled()
                    .onSubmit { model.query(queryText) }
                    .disabled(model.busy)
                if model.results.isEmpty {
                    hostsSection
                    Spacer()
                    Text(model.status)
                        .font(Brand.font(.callout))
                        .foregroundStyle(.secondary)
                    ForEach(model.parkedWarnings, id: \.self) { warning in
                        Text(warning).font(Brand.font(.caption)).foregroundStyle(.orange)
                    }
                    Spacer()
                } else {
                    List(model.results) { result in
                        QueryResultRow(result: result)
                    }
                    .listStyle(.plain)
                    Text(model.status).font(Brand.font(.caption)).foregroundStyle(.secondary)
                }
            }
            .padding()
            .background(Brand.ground)
            .foregroundStyle(Brand.node)
            .navigationTitle("inseam")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    if model.busy {
                        ProgressView()
                    } else {
                        Button("attach recording") { model.beginAttach(fileURL: nil) }
                            .font(Brand.font(.caption))
                    }
                }
            }
            .sheet(item: $model.pendingAttachment) { attachment in
                AttachRecordingView(attachment: attachment)
            }
        }
        .preferredColorScheme(.dark)
    }

    private var hostsSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("this device stewards").font(Brand.font(.caption)).foregroundStyle(.secondary)
            ForEach(model.hosts) { host in
                HStack {
                    Text("●").foregroundStyle(Brand.node)
                    Text(host.displayName).font(Brand.font(.body))
                    Spacer()
                    Text(host.kind).font(Brand.font(.caption2)).foregroundStyle(.secondary)
                }
            }
            HStack {
                Button("index photos") { model.indexPhotos() }
                Button("index call recordings") { model.indexCallRecordings() }
            }
            .font(Brand.font(.callout))
            .buttonStyle(.bordered)
            .tint(Brand.thread)
            .disabled(!model.nodeOpen || model.busy)
        }
    }
}

/// Attach a recording to a call: pick the file (unless the share sheet
/// already handed us one), name the participant — typed, or from
/// Contacts — and import. The call's times come from the observer when
/// it saw one end recently.
struct AttachRecordingView: View {
    @Environment(NodeModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State var attachment: PendingAttachment
    @State private var participant = ""
    @State private var showingPicker = false
    @State private var showingContacts = false

    var body: some View {
        NavigationStack {
            Form {
                Section("recording") {
                    if let url = attachment.fileURL {
                        Text(url.lastPathComponent).font(Brand.font(.body))
                    } else {
                        Button("choose audio or transcript…") { showingPicker = true }
                    }
                    if let call = attachment.call {
                        Text("call \(call.outgoing ? "to" : "from") someone, \(call.started.formatted(date: .abbreviated, time: .shortened))")
                            .font(Brand.font(.caption))
                            .foregroundStyle(.secondary)
                    }
                }
                Section("participant") {
                    TextField("phone number or name", text: $participant)
                        .font(Brand.font(.body))
                        .keyboardType(.phonePad)
                    Button("from contacts…") { showingContacts = true }
                }
            }
            .navigationTitle("attach recording")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("attach") {
                        model.finishAttach(attachment, participant: participant)
                        dismiss()
                    }
                    .disabled(attachment.fileURL == nil || participant.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .fileImporter(isPresented: $showingPicker, allowedContentTypes: [.audio, .plainText]) { result in
                if case .success(let url) = result {
                    attachment.fileURL = url
                }
            }
            .sheet(isPresented: $showingContacts) {
                ContactPicker { chosen in participant = chosen }
            }
        }
        .font(Brand.font(.body))
    }
}
