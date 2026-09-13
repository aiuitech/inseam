import AVFoundation
import InseamKit
import SwiftUI

struct MeetingRecordingView: View {
    @Environment(NodeModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var recorder = MeetingRecorder()
    @State private var name = ""

    var body: some View {
        NavigationStack {
            Form {
                Section("meeting name") {
                    TextField("optional name", text: $name)
                        .disabled(recorder.active)
                        .onChange(of: name) { _, value in name = String(value.prefix(200)) }
                }
                Section {
                    recordingControls
                } footer: {
                    Text("Let everyone know you are recording. Place the phone near the conversation and keep it still. Recording can continue with the screen locked, for up to four hours.")
                }
            }
            .font(Brand.font(.body))
            .tint(Brand.thread)
            .navigationTitle("record a meeting")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("done") { dismiss() }.disabled(recorder.active)
                }
            }
        }
        .interactiveDismissDisabled(recorder.active)
        .onChange(of: recorder.savedURL) { _, url in
            if let url { model.processMeeting(at: url) }
        }
    }

    @ViewBuilder
    private var recordingControls: some View {
        switch recorder.state {
        case .ready:
            Text("Preserves stereo on supported devices for later speaker analysis. People are not identified automatically.")
            startButton
        case .requestingPermission:
            ProgressView("preparing microphone…")
        case .recording(let started, let audio):
            Label("recording", systemImage: "record.circle.fill").foregroundStyle(.red)
            Text(started, style: .timer).monospacedDigit()
            Text(audio.channels == 2 ? "stereo · keep the phone still" : "mono · stereo unavailable on this device")
                .font(Brand.font(.caption))
            Button("stop and save", role: .destructive) { recorder.stop() }
        case .saved(let url, let reason):
            Label("recording saved", systemImage: "checkmark.circle")
            Text(Self.stopMessage(reason)).font(Brand.font(.caption))
            ShareLink("share audio", item: url)
            Text(model.status).font(Brand.font(.caption)).foregroundStyle(.secondary)
        case .failed(let message):
            Text(message).foregroundStyle(.orange)
            startButton
        }
    }

    private var startButton: some View {
        Button("start recording") {
            let orientation = Self.inputOrientation()
            Task { await recorder.start(name: name, folder: model.recordings.folder, orientation: orientation) }
        }
    }

    private static func inputOrientation() -> AVAudioSession.StereoOrientation {
        let scene = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
            .first { $0.activationState == .foregroundActive }
        switch scene?.effectiveGeometry.interfaceOrientation {
        case .landscapeLeft: return .landscapeLeft
        case .landscapeRight: return .landscapeRight
        case .portraitUpsideDown: return .portraitUpsideDown
        default: return .portrait
        }
    }

    private static func stopMessage(_ reason: MeetingMetadata.StopReason) -> String {
        switch reason {
        case .user: return "Original audio is saved in Files › inseam › Call Recordings."
        case .durationLimit: return "Stopped at the four-hour recording limit."
        case .interruption: return "An audio interruption stopped the recording. Start a new recording to continue."
        case .routeChange: return "The microphone changed, so recording stopped to preserve channel consistency."
        case .mediaReset: return "The audio service restarted. Audio captured before the restart was saved."
        case .encodingFailure: return "The encoder stopped. Check the saved audio for missing content."
        }
    }
}
