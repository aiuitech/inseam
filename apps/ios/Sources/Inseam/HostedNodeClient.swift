import Foundation
import InseamKit

/// The phone's thin client for the owner's always-on node
/// (design/call-capture.md). Call capture is placed by that node, never
/// by the phone: the number, the Twilio key, and the archive all live
/// there, so the phone only drives its `call-capture` operations over the
/// node's HTTP owner API. The owner token is exchanged for a session
/// cookie once per process, exactly as the web console does.
actor HostedNodeClient {
    struct Configuration: Equatable {
        var baseURL: URL
        var ownerToken: String
    }

    struct ClientError: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    /// Where the hosted node's URL is remembered; the token is Keychain-only.
    static let baseURLDefaultsKey = "hosted.node.url"
    static let ownerTokenSecretName = "INSEAM_HOSTED_OWNER_TOKEN"
    private static let requestTimeoutSeconds: TimeInterval = 20
    private static let bodyBytesMax = 1 << 20

    private let configuration: Configuration
    private let session: URLSession
    private var loggedIn = false

    init(configuration: Configuration) {
        self.configuration = configuration
        let sessionConfiguration = URLSessionConfiguration.ephemeral
        sessionConfiguration.timeoutIntervalForRequest = Self.requestTimeoutSeconds
        sessionConfiguration.httpCookieAcceptPolicy = .always
        session = URLSession(configuration: sessionConfiguration)
    }

    /// The saved configuration, or nil until the owner enters both parts
    /// in settings.
    static func savedConfiguration() -> Configuration? {
        guard
            let raw = UserDefaults.standard.string(forKey: baseURLDefaultsKey),
            let url = URL(string: raw), url.scheme == "https" || url.scheme == "http",
            let token = try? SecretStore.read(name: ownerTokenSecretName), !token.isEmpty
        else { return nil }
        return Configuration(baseURL: url, ownerToken: token)
    }

    func captureStatus() async throws -> CaptureStatus {
        try await request("GET", "/api/v1/owner/capture")
    }

    func setCaptureNumber(_ number: String) async throws -> CaptureStatus {
        try await request("POST", "/api/v1/owner/capture/number", body: ["number": number])
    }

    func verifyCaptureNumber(_ code: String) async throws -> CaptureStatus {
        try await request("POST", "/api/v1/owner/capture/verify", body: ["code": code])
    }

    func startCallCapture() async throws -> CaptureStatus {
        try await request("POST", "/api/v1/owner/capture/start")
    }

    private func login() async throws {
        let (data, response) = try await send("POST", "/api/v1/session", body: ["token": configuration.ownerToken])
        guard (200..<300).contains(response.statusCode) else {
            throw ClientError(message: Self.message(in: data) ?? "the node refused the owner token")
        }
        loggedIn = true
    }

    private func request<T: Decodable>(_ method: String, _ path: String, body: [String: String]? = nil) async throws -> T {
        if !loggedIn { try await login() }
        var (data, response) = try await send(method, path, body: body)
        if response.statusCode == 401 {
            // The 12-hour session lapsed: log in once more, then retry once.
            loggedIn = false
            try await login()
            (data, response) = try await send(method, path, body: body)
        }
        guard (200..<300).contains(response.statusCode) else {
            throw ClientError(message: Self.message(in: data) ?? "the node answered \(response.statusCode)")
        }
        do {
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            throw ClientError(message: "the node's answer was not readable: \(error.localizedDescription)")
        }
    }

    private func send(_ method: String, _ path: String, body: [String: String]?) async throws -> (Data, HTTPURLResponse) {
        var request = URLRequest(url: configuration.baseURL.appendingPathComponent(path))
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw ClientError(message: "no HTTP response from the node")
        }
        guard data.count <= Self.bodyBytesMax else {
            throw ClientError(message: "the node's answer was larger than expected")
        }
        return (data, http)
    }

    private static func message(in data: Data) -> String? {
        struct Failure: Decodable { let error: String? ; let message: String? }
        guard let failure = try? JSONDecoder().decode(Failure.self, from: data) else { return nil }
        return failure.error ?? failure.message
    }
}

/// `CaptureStatus` as the node's `call-capture` seam serializes it.
struct CaptureStatus: Decodable, Equatable {
    struct Call: Decodable, Equatable {
        let id: String
        let startedAt: Int64
        enum CodingKeys: String, CodingKey {
            case id
            case startedAt = "started_at"
        }
    }

    enum OwnerNumber: Equatable {
        case unset
        case pending(number: String, expiresAt: Int64)
        case verified(number: String)
    }

    let captureNumber: String
    let ownerNumber: OwnerNumber
    let lastCall: Call?
    let recordingsArchived: UInt64

    enum CodingKeys: String, CodingKey {
        case captureNumber = "capture_number"
        case ownerNumber = "owner_number"
        case lastCall = "last_call"
        case recordingsArchived = "recordings_archived"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        captureNumber = try container.decode(String.self, forKey: .captureNumber)
        lastCall = try container.decodeIfPresent(Call.self, forKey: .lastCall)
        recordingsArchived = try container.decode(UInt64.self, forKey: .recordingsArchived)
        let owner = try container.nestedContainer(keyedBy: OwnerKeys.self, forKey: .ownerNumber)
        switch try owner.decode(String.self, forKey: .state) {
        case "pending":
            ownerNumber = .pending(
                number: try owner.decode(String.self, forKey: .number),
                expiresAt: try owner.decode(Int64.self, forKey: .expiresAt)
            )
        case "verified":
            ownerNumber = .verified(number: try owner.decode(String.self, forKey: .number))
        default:
            ownerNumber = .unset
        }
    }

    private enum OwnerKeys: String, CodingKey {
        case state, number
        case expiresAt = "expires_at"
    }
}
