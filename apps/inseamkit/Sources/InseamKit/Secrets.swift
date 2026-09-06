import Foundation
import LocalAuthentication
import Security

/// The app's Keychain-backed secret store. Each item is one environment
/// variable the composition references by name (`api_key_env`-style):
/// the Keychain account is the variable name, the Keychain value is the
/// secret. The composition file itself never holds secrets
/// (design/composition.md) — the shell provides the environment for the
/// CLI, and this store provides it for the app.
public enum SecretStore {
    /// Keychain service namespace for every item this app owns.
    private static let service = "app.inseam.secrets"
    private static let secretCountMax = 64

    public struct SecretStoreError: LocalizedError {
        public let message: String
        public var errorDescription: String? { message }
    }

    /// Names of every stored secret, sorted so the UI order is stable.
    public static func names() throws -> [String] {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecMatchLimit as String: kSecMatchLimitAll,
            kSecReturnAttributes as String: true,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound {
            return []
        }
        guard status == errSecSuccess, let items = result as? [[String: Any]] else {
            throw SecretStoreError(message: "keychain list failed (status \(status))")
        }
        return items.compactMap { $0[kSecAttrAccount as String] as? String }.sorted()
    }

    /// The secret stored under `name`, or nil when none exists.
    public static func read(name: String) throws -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: name,
            kSecMatchLimit as String: kSecMatchLimitOne,
            kSecReturnData as String: true,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess, let data = result as? Data else {
            throw SecretStoreError(message: "keychain read of `\(name)` failed (status \(status))")
        }
        guard let value = String(data: data, encoding: .utf8) else {
            throw SecretStoreError(message: "keychain item `\(name)` is not UTF-8")
        }
        return value
    }

    /// Store `value` under `name`, replacing any existing item.
    public static func write(name: String, value: String) throws {
        try delete(name: name)
        let attributes: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: name,
            kSecValueData as String: Data(value.utf8),
        ]
        let status = SecItemAdd(attributes as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw SecretStoreError(message: "keychain write of `\(name)` failed (status \(status))")
        }
    }

    /// Remove the item stored under `name`; a missing item is not an error.
    public static func delete(name: String) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: name,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw SecretStoreError(message: "keychain delete of `\(name)` failed (status \(status))")
        }
    }

    /// Export every stored secret into this process's environment so the
    /// core's `key_env`-style config fields resolve when the node opens.
    public static func exportIntoEnvironment() throws {
        let storedNames = try names()
        guard storedNames.count <= secretCountMax else {
            throw SecretStoreError(message: "keychain contains too many Inseam secrets")
        }
        for name in storedNames {
            guard let value = try read(name: name) else { continue }
            setenv(name, value, 1)
        }
    }

    /// Look up only the exact Keychain service names declared missing by
    /// plugins. This supports conventional items named after their env var
    /// without enumerating unrelated Keychain entries or showing access UI.
    /// Returns true when at least one value was exported.
    public static func exportDeclaredIntoEnvironment(names: [String]) throws -> Bool {
        let names = Array(Set(names)).sorted()
        guard names.count <= secretCountMax else { return false }
        var exported = false
        for name in names where isValidName(name) {
            guard getenv(name) == nil else { continue }
            guard let value = try readConventional(name: name) else { continue }
            setenv(name, value, 1)
            exported = true
        }
        return exported
    }

    /// Read one conventional generic-password item whose service is exactly
    /// the environment variable name. Authentication UI is disabled because
    /// automatic startup must never fan out into Keychain permission dialogs.
    private static func readConventional(name: String) throws -> String? {
        guard let account = try uniqueAccount(service: name) else { return nil }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: name,
            kSecAttrAccount as String: account,
            kSecMatchLimit as String: kSecMatchLimitOne,
            kSecReturnData as String: true,
            kSecUseAuthenticationContext as String: noninteractiveContext(),
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if inaccessible(status) { return nil }
        guard status == errSecSuccess, let data = result as? Data else {
            throw SecretStoreError(message: "keychain read of service `\(name)` failed (status \(status))")
        }
        guard let value = String(data: data, encoding: .utf8) else {
            throw SecretStoreError(message: "keychain service `\(name)` is not UTF-8")
        }
        return value
    }

    /// Resolve the account before requesting secret data. More than one item
    /// under the declared service is ambiguous, so startup leaves it missing.
    private static func uniqueAccount(service: String) throws -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecMatchLimit as String: kSecMatchLimitAll,
            kSecReturnAttributes as String: true,
            kSecUseAuthenticationContext as String: noninteractiveContext(),
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if inaccessible(status) { return nil }
        guard status == errSecSuccess else {
            throw SecretStoreError(
                message: "keychain lookup of service `\(service)` failed (status \(status))"
            )
        }
        let items = result as? [[String: Any]]
        guard let items, items.count == 1 else { return nil }
        return items[0][kSecAttrAccount as String] as? String
    }

    private static func noninteractiveContext() -> LAContext {
        let context = LAContext()
        context.interactionNotAllowed = true
        return context
    }

    private static func inaccessible(_ status: OSStatus) -> Bool {
        switch status {
        case errSecItemNotFound, errSecAuthFailed, errSecInteractionNotAllowed,
             errSecUserCanceled:
            true
        default:
            false
        }
    }

    /// A valid environment variable name: nonempty ASCII letters, digits,
    /// and underscores, not starting with a digit.
    public static func isValidName(_ name: String) -> Bool {
        guard let first = name.unicodeScalars.first else { return false }
        guard CharacterSet(charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_")
            .contains(first) else { return false }
        let allowed = CharacterSet(
            charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_"
        )
        return name.unicodeScalars.allSatisfy { allowed.contains($0) }
    }
}
