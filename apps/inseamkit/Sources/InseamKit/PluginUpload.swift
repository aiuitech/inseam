import Foundation

/// These mirror `inseam_seams::operations`; keep them equal so the app
/// refuses oversized work before allocating the JSON request.
public let PLUGIN_FILES_MAX: UInt32 = 64
public let PLUGIN_UPLOAD_BYTES_MAX: UInt64 = 32 * 1024 * 1024
public let PLUGIN_ID_CHARS_MAX: UInt32 = 64

/// Bounds traversal even when a directory contains thousands of empty
/// folders or non-regular entries that do not count toward the upload.
private let PLUGIN_DIRECTORY_ENTRIES_MAX: UInt32 = 4_096

public struct PluginDirectoryInspection: Identifiable {
    public let directory: URL
    public let files: [URL]
    public let artifact: URL
    public let bytesTotal: UInt64

    public var id: URL { directory }

    public var suggestedId: String {
        artifact.deletingPathExtension().lastPathComponent.lowercased()
    }
}

private struct PluginIdentifier {
    let value: String

    init(_ value: String) throws {
        guard !value.isEmpty else { throw CoreError(message: "plugin id is empty") }
        guard value.count <= Int(PLUGIN_ID_CHARS_MAX) else {
            throw CoreError(
                message: "plugin id `\(value)` is longer than \(PLUGIN_ID_CHARS_MAX) characters"
            )
        }
        for character in value.unicodeScalars {
            guard Self.characterIsAllowed(character.value) else {
                throw CoreError(
                    message:
                        "plugin id `\(value)` may only use lowercase letters, digits, `-` and `_`"
                )
            }
        }
        guard let first = value.unicodeScalars.first else {
            throw CoreError(message: "plugin id is empty")
        }
        guard Self.characterStartsId(first.value) else {
            throw CoreError(message: "plugin id `\(value)` must start with a letter or digit")
        }
        self.value = value
    }

    private static func characterIsAllowed(_ value: UInt32) -> Bool {
        if (97...122).contains(value) { return true }
        if (48...57).contains(value) { return true }
        if value == 45 { return true }
        return value == 95
    }

    private static func characterStartsId(_ value: UInt32) -> Bool {
        if (97...122).contains(value) { return true }
        return (48...57).contains(value)
    }
}

public enum PluginUpload {
    public static func pluginIdProblem(_ id: String) -> String? {
        do {
            _ = try PluginIdentifier(id)
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    public static func inspect(directory: URL) throws -> PluginDirectoryInspection {
        let directory = directory.standardizedFileURL
        let keys: [URLResourceKey] = [.isRegularFileKey, .fileSizeKey]
        var traversalError: Error?
        guard let enumerator = FileManager.default.enumerator(
            at: directory,
            includingPropertiesForKeys: keys,
            options: [.skipsHiddenFiles],
            errorHandler: { _, error in
                traversalError = error
                return false
            }
        ) else {
            throw CoreError(message: "could not read plugin directory `\(directory.path)`")
        }
        let files = try inspectFiles(enumerator: enumerator, traversalError: &traversalError)
        let artifacts = files.urls.filter { $0.pathExtension == "wasm" }
        guard artifacts.count == 1 else {
            throw artifactCountError(artifacts.count)
        }
        let sorted = files.urls.sorted {
            relativePath($0, in: directory) < relativePath($1, in: directory)
        }
        return PluginDirectoryInspection(
            directory: directory,
            files: sorted,
            artifact: artifacts[0],
            bytesTotal: files.bytesTotal
        )
    }

    public static func request(id: String, directory: URL) throws -> InstallPluginRequest {
        let id = try PluginIdentifier(id)
        let inspection = try inspect(directory: directory)
        var bytesTotal: UInt64 = 0
        var files: [PluginFile] = []
        files.reserveCapacity(inspection.files.count)
        for file in inspection.files {
            let data = try Data(contentsOf: file, options: [.mappedIfSafe])
            try add(size: UInt64(data.count), to: &bytesTotal)
            files.append(
                PluginFile(
                    path: relativePath(file, in: inspection.directory),
                    bytes: data.base64EncodedString()
                )
            )
        }
        assert(files.count <= Int(PLUGIN_FILES_MAX))
        assert(bytesTotal <= PLUGIN_UPLOAD_BYTES_MAX)
        return InstallPluginRequest(id: id.value, files: files)
    }

    private static func inspectFiles(
        enumerator: FileManager.DirectoryEnumerator,
        traversalError: inout Error?
    ) throws -> (urls: [URL], bytesTotal: UInt64) {
        var entriesCount: UInt32 = 0
        var files: [URL] = []
        var bytesTotal: UInt64 = 0
        for case let file as URL in enumerator {
            guard entriesCount < PLUGIN_DIRECTORY_ENTRIES_MAX else {
                throw CoreError(
                    message: "a plugin directory may contain at most \(PLUGIN_DIRECTORY_ENTRIES_MAX) entries"
                )
            }
            entriesCount += 1
            let values = try file.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
            guard values.isRegularFile == true else { continue }
            guard files.count < Int(PLUGIN_FILES_MAX) else {
                throw CoreError(
                    message: "a plugin upload may carry at most \(PLUGIN_FILES_MAX) files"
                )
            }
            guard let fileSize = values.fileSize else {
                throw CoreError(message: "could not read the size of `\(file.path)`")
            }
            guard fileSize >= 0 else {
                throw CoreError(message: "`\(file.path)` has a negative file size")
            }
            try add(size: UInt64(fileSize), to: &bytesTotal)
            files.append(file)
        }
        if let traversalError { throw traversalError }
        return (files, bytesTotal)
    }

    private static func add(size: UInt64, to total: inout UInt64) throws {
        guard size <= PLUGIN_UPLOAD_BYTES_MAX else {
            throw CoreError(
                message: "a plugin upload may carry at most \(PLUGIN_UPLOAD_BYTES_MAX) bytes"
            )
        }
        guard total <= PLUGIN_UPLOAD_BYTES_MAX - size else {
            throw CoreError(
                message: "a plugin upload may carry at most \(PLUGIN_UPLOAD_BYTES_MAX) bytes"
            )
        }
        total += size
    }

    private static func artifactCountError(_ count: Int) -> CoreError {
        if count == 0 {
            return CoreError(message: "the plugin directory has no .wasm artifact")
        }
        return CoreError(
            message: "the plugin directory must contain exactly one .wasm artifact; found \(count)"
        )
    }

    private static func relativePath(_ file: URL, in directory: URL) -> String {
        let root = directory.standardizedFileURL.pathComponents
        let path = file.standardizedFileURL.pathComponents
        assert(path.starts(with: root))
        assert(path.count > root.count)
        return path.dropFirst(root.count).joined(separator: "/")
    }
}
