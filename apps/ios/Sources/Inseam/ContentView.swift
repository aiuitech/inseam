import InseamKit
import SwiftUI

/// The client: a search field, results, and the two things this device
/// stewards. Brand: dark ground, chartreuse thread, monospace everywhere.
struct ContentView: View {
    @Environment(NodeModel.self) private var model
    @State private var queryText = ""
    @State private var showingSettings = false
    @State private var showingRecorder = false
    @State private var showingCapture = false

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                StitchStrip(height: 10)
                    .foregroundStyle(Brand.thread)
                HStack {
                    meetingButton
                    captureButton
                }
                TextField("search your data…", text: $queryText)
                    .textFieldStyle(.roundedBorder)
                    .font(Brand.font(.body))
                    .autocorrectionDisabled()
                    .onSubmit { model.query(queryText) }
                    .disabled(model.busy)
                if model.indexProgress != nil {
                    IndexingProcessView()
                }
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
                ToolbarItemGroup(placement: .topBarTrailing) {
                    if model.busy {
                        ProgressView()
                    } else {
                        Button("attach recording") { model.beginAttach(fileURL: nil) }
                            .font(Brand.font(.caption))
                    }
                    Button {
                        showingSettings = true
                    } label: {
                        Image(systemName: "gearshape")
                    }
                }
            }
            .sheet(item: $model.pendingAttachment) { attachment in
                AttachRecordingView(attachment: attachment)
            }
            .sheet(isPresented: $showingRecorder) {
                MeetingRecordingView().environment(model)
            }
            .sheet(isPresented: $showingCapture) {
                CallCaptureView().environment(model)
            }
            .sheet(isPresented: $showingSettings) {
                SettingsView()
                    .environment(model)
            }
        }
        .preferredColorScheme(.dark)
    }

    private var meetingButton: some View {
        Button {
            showingRecorder = true
        } label: {
            Label("record a meeting", systemImage: "mic.fill")
                .font(Brand.font(.body))
        }
        .buttonStyle(.borderedProminent)
        .tint(Brand.thread)
        .foregroundStyle(Brand.ground)
        .disabled(model.busy)
    }

    /// Call capture is the hosted node's to place; the phone only asks.
    private var captureButton: some View {
        Button {
            showingCapture = true
        } label: {
            Label("capture this call", systemImage: "phone.badge.plus")
                .font(Brand.font(.body))
        }
        .buttonStyle(.bordered)
        .tint(Brand.thread)
        .disabled(model.busy)
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
                Button("index recordings") { model.indexCallRecordings() }
            }
            .font(Brand.font(.callout))
            .buttonStyle(.bordered)
            .tint(Brand.thread)
            .disabled(!model.nodeOpen || model.busy)
        }
    }
}

private struct IndexingProcessView: View {
    @Environment(NodeModel.self) private var model

    var body: some View {
        if let progress = model.indexProgress {
            VStack(alignment: .leading, spacing: 9) {
                HStack {
                    Label(progress.phase.label.lowercased(), systemImage: "square.stack.3d.up")
                        .font(Brand.font(.headline))
                    Spacer()
                    Text(countLabel(progress))
                        .font(Brand.font(.caption))
                        .foregroundStyle(.secondary)
                }
                if let fraction = progress.fractionCompleted {
                    ProgressView(value: fraction).tint(Brand.thread)
                } else {
                    ProgressView().tint(Brand.thread)
                }
                if let current = progress.current {
                    Text(current)
                        .font(Brand.font(.caption2))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                HStack {
                    Text("\(progress.indexed) indexed · \(progress.unchanged) unchanged")
                        .font(Brand.font(.caption))
                        .foregroundStyle(.secondary)
                    Spacer()
                    controls
                }
            }
            .padding(12)
            .background(Brand.node.opacity(0.08), in: RoundedRectangle(cornerRadius: 12))
        }
    }

    @ViewBuilder
    private var controls: some View {
        if model.indexingActive {
            if model.indexStopping {
                Text("stopping…").font(Brand.font(.caption))
            } else if model.indexPaused {
                Button("resume") { model.resumeIndexing() }
                Button("stop") { model.stopIndexing() }
            } else {
                Button("pause") { model.pauseIndexing() }
                Button("stop") { model.stopIndexing() }
            }
        } else {
            Button("dismiss") { model.dismissIndexProgress() }
        }
    }

    private func countLabel(_ progress: IndexProgress) -> String {
        guard progress.sourcesTotal > 0 else { return "discovering" }
        return "\(progress.sourcesComplete) / \(progress.sourcesTotal)"
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
