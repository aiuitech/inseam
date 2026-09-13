import AVFoundation
import InseamKit
import XCTest

final class MeetingRecordingTests: XCTestCase {
    func testLegacyCallSidecarRemainsReadable() throws {
        let folder = try temporaryFolder()
        defer { try? FileManager.default.removeItem(at: folder) }
        let audio = folder.appendingPathComponent("call.m4a")
        try Data([1]).write(to: audio)
        let json = #"{"participant":"Ada","callStarted":"2026-01-01T00:00:00Z"}"#
        try Data(json.utf8).write(to: folder.appendingPathComponent("call.json"))
        let sources = try CallRecordingsHost(folder: folder).enumerate(root: "")
        let source = try XCTUnwrap(sources.first)
        XCTAssertEqual(source.sourceType, "call-recording")
        XCTAssertTrue(source.title?.contains("Ada") == true)
        XCTAssertNil(Sidecar.read(beside: audio)?.meeting)
    }

    func testMeetingAudioAndTranscriptRetainCaptureMetadata() throws {
        let folder = try temporaryFolder()
        defer { try? FileManager.default.removeItem(at: folder) }
        let audio = folder.appendingPathComponent("meeting.caf")
        try Data([1, 2]).write(to: audio)
        try Data("hello".utf8).write(to: folder.appendingPathComponent("meeting.txt"))
        let metadata = MeetingMetadata(id: UUID(), name: "Planning", audio: capture(), stopReason: .user)
        try Sidecar(participant: "", callStarted: Date(timeIntervalSince1970: 0), meeting: metadata).write(beside: audio)
        let sources = try CallRecordingsHost(folder: folder).enumerate(root: "")
        XCTAssertEqual(Set(sources.map(\.sourceType)), ["meeting-recording", "meeting-transcript"])
        XCTAssertEqual(sources.count, 2)
        for source in sources.prefix(2) {
            XCTAssertTrue(source.title?.hasPrefix("Planning,") == true)
            XCTAssertFalse(source.properties.contains { $0.key == "participant" })
            XCTAssertTrue(source.properties.contains { $0.key == "audio-channels" && $0.value == "2" })
        }
        let restored = try XCTUnwrap(Sidecar.read(beside: audio)?.meeting)
        XCTAssertEqual(restored.audio, metadata.audio)
        XCTAssertEqual(restored.id, metadata.id)
        XCTAssertEqual(try CallRecordingsHost(folder: folder).readBytes(locator: "meeting.caf"), Data([1, 2]))
    }

    func testLosslessCaptureKeepsDistinctChannels() throws {
        let folder = try temporaryFolder()
        defer { try? FileManager.default.removeItem(at: folder) }
        let url = folder.appendingPathComponent("channels.caf")
        let format = try XCTUnwrap(AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2))
        let buffer = try XCTUnwrap(AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 128))
        buffer.frameLength = 128
        let channels = try XCTUnwrap(buffer.floatChannelData)
        for index in 0..<128 {
            channels[0][index] = 0.25
            channels[1][index] = -0.5
        }
        do {
            let file = try AVAudioFile(forWriting: url, settings: MeetingAudioSession.settings(for: capture()))
            try file.write(from: buffer)
        }
        let file = try AVAudioFile(forReading: url)
        XCTAssertEqual(file.fileFormat.channelCount, 2)
        XCTAssertEqual(file.fileFormat.sampleRate, 48_000)
        let restored = try XCTUnwrap(AVAudioPCMBuffer(pcmFormat: file.processingFormat, frameCapacity: 128))
        try file.read(into: restored)
        let restoredChannels = try XCTUnwrap(restored.floatChannelData)
        XCTAssertEqual(restoredChannels[0][0], 0.25, accuracy: 0.0001)
        XCTAssertEqual(restoredChannels[1][0], -0.5, accuracy: 0.0001)
    }

    func testUnfinishedMeetingIsMarkedIncomplete() throws {
        let folder = try temporaryFolder()
        defer { try? FileManager.default.removeItem(at: folder) }
        let audio = folder.appendingPathComponent("partial.caf")
        try Data([1]).write(to: audio)
        var mono = capture()
        mono.channels = 1
        let metadata = MeetingMetadata(id: UUID(), name: "Unfinished", audio: mono)
        try Sidecar(participant: "", callStarted: Date(timeIntervalSince1970: 0), meeting: metadata).write(beside: audio)
        let source = try XCTUnwrap(CallRecordingsHost(folder: folder).enumerate(root: "").first)
        XCTAssertTrue(source.properties.contains { $0.key == "capture-state" && $0.value == "incomplete" })
        XCTAssertEqual(Sidecar.read(beside: audio)?.meeting?.audio.channels, 1)
    }

    func testRejectsRecordingOutsideHostFolder() throws {
        let host = CallRecordingsHost(folder: FileManager.default.temporaryDirectory)
        XCTAssertThrowsError(try host.readBytes(locator: "../outside.caf")) { error in
            guard case RecordingsError.badLocator = error else {
                return XCTFail("Expected a rejected locator.")
            }
        }
    }

    private func capture() -> MeetingAudioMetadata {
        MeetingAudioMetadata(channels: 2, sampleRate: 48_000, inputUID: "built-in", inputName: "iPhone",
                             dataSource: "Front", polarPattern: "Stereo", inputOrientation: 1)
    }

    private func temporaryFolder() throws -> URL {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        return folder
    }
}
