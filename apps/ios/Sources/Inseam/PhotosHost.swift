import Foundation
import InseamKit
import Photos
import UniformTypeIdentifiers

/// The device's photo library as a bridged host (design/ios-app.md).
/// Locators are `photos/<PHAsset local identifier>`; the envelope carries
/// what PhotoKit exposes without reading pixels — dates, place, favorite,
/// kind — and the bytes are the original image data on demand (iCloud
/// originals are fetched when needed). Captions, people, and Photos'
/// own search are not available to third-party apps, so they are not
/// promised here.
///
/// Images only for now: the node has no video transform, and a video's
/// bytes are far past the bridge's read ceiling.
final class PhotosHost: BridgedHostSource, @unchecked Sendable {
    static let kind = "photos"

    /// Most assets one enumeration lists; a larger library is indexed in
    /// runs (the sweep is resumable), never in one unbounded fetch.
    static let assetsPerEnumerationMax = 50_000

    let description: BridgedHostDescription

    private init() {
        // The library's identity is the device's, not the app's: a second
        // steward of the same device (none today) would derive the same id.
        let device = ProcessInfo.processInfo.hostName
        description = BridgedHostDescription(
            kind: Self.kind,
            principal: "photos:\(device)",
            displayName: "Photos on \(device)"
        )
    }

    /// The host when full read access is already granted, else nil.
    static func ifAuthorized() -> PhotosHost? {
        PHPhotoLibrary.authorizationStatus(for: .readWrite) == .authorized ? PhotosHost() : nil
    }

    /// Ask for full access (limited access would index a subset silently,
    /// so it is refused by name) and return the host.
    static func requestingAccess() throws -> PhotosHost {
        let semaphore = DispatchSemaphore(value: 0)
        var granted = PHAuthorizationStatus.notDetermined
        PHPhotoLibrary.requestAuthorization(for: .readWrite) { status in
            granted = status
            semaphore.signal()
        }
        semaphore.wait()
        switch granted {
        case .authorized:
            return PhotosHost()
        case .limited:
            throw PhotosError.limitedAccess
        default:
            throw PhotosError.denied
        }
    }

    func enumerate(root: String) throws -> [BridgedSource] {
        let options = PHFetchOptions()
        options.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: false)]
        options.fetchLimit = Self.assetsPerEnumerationMax
        let assets = PHAsset.fetchAssets(with: .image, options: options)
        var sources: [BridgedSource] = []
        sources.reserveCapacity(assets.count)
        assets.enumerateObjects { asset, _, _ in
            let locator = Self.locator(for: asset)
            guard locator.hasPrefix(root) else { return }
            sources.append(Self.source(for: asset, locator: locator))
        }
        assert(sources.count <= Self.assetsPerEnumerationMax)
        return sources
    }

    func readBytes(locator: String) throws -> Data {
        guard locator.hasPrefix("photos/") else { throw PhotosError.notAPhoto(locator) }
        let identifier = String(locator.dropFirst("photos/".count))
        guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [identifier], options: nil).firstObject else {
            throw PhotosError.missing(identifier)
        }
        let options = PHImageRequestOptions()
        options.isSynchronous = true
        options.isNetworkAccessAllowed = true
        options.deliveryMode = .highQualityFormat
        options.version = .current
        var data: Data?
        var failure: Error?
        PHImageManager.default().requestImageDataAndOrientation(for: asset, options: options) { bytes, _, _, info in
            data = bytes
            failure = info?[PHImageErrorKey] as? Error
        }
        if let failure { throw failure }
        guard let data else { throw PhotosError.missing(identifier) }
        return data
    }

    static func locator(for asset: PHAsset) -> String {
        "photos/\(asset.localIdentifier)"
    }

    private static func source(for asset: PHAsset, locator: String) -> BridgedSource {
        let resource = PHAssetResource.assetResources(for: asset).first
        let bytes = (resource?.value(forKey: "fileSize") as? NSNumber)?.uint64Value ?? 0
        let contentType = resource.flatMap { UTType($0.uniformTypeIdentifier)?.preferredMIMEType } ?? "image/jpeg"
        var properties: [BridgedProperty] = []
        if let location = asset.location {
            properties.append(BridgedProperty(
                key: "place",
                value: String(format: "%.5f,%.5f", location.coordinate.latitude, location.coordinate.longitude)
            ))
        }
        if asset.isFavorite {
            properties.append(BridgedProperty(key: "favorite", value: "true"))
        }
        if asset.mediaSubtypes.contains(.photoScreenshot) {
            properties.append(BridgedProperty(key: "kind", value: "screenshot"))
        }
        let created = asset.creationDate.map { Int64($0.timeIntervalSince1970) }
        return BridgedSource(
            locator: locator,
            sourceType: "photo",
            contentType: contentType,
            bytes: bytes,
            created: created,
            modified: asset.modificationDate.map { Int64($0.timeIntervalSince1970) },
            title: asset.creationDate.map { Self.titleFormatter.string(from: $0) },
            properties: properties
        )
    }

    private static let titleFormatter: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateStyle = .long
        formatter.timeStyle = .short
        return formatter
    }()
}

enum PhotosError: LocalizedError {
    case denied
    case limitedAccess
    case notAPhoto(String)
    case missing(String)

    var errorDescription: String? {
        switch self {
        case .denied:
            return "photo library access was not granted; allow it in Settings › Privacy › Photos"
        case .limitedAccess:
            return "photo library access is limited to a selection; choose Full Access to index the library"
        case .notAPhoto(let locator):
            return "\(locator) is not a photo locator"
        case .missing(let identifier):
            return "photo \(identifier) is no longer in the library"
        }
    }
}
