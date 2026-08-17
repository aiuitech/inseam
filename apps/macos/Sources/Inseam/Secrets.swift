import Foundation
import Security

/// The app's Keychain-backed secret store. Each item is one environment
/// variable the composition references by name (`api_key_env`-style):
/// the Keychain account is the variable name, the Keychain value is the
/// secret. The composition file itself never holds secrets
/// (design/composition.md) — the shell provides the environment for the
/// CLI, and this store provides it for the app.
enum SecretStore {
    /// Keychain service namespace for every item this app owns.
    private static let service = "app.inseam.secrets"

    struct SecretStoreError: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    /// Names of every stored secret, sorted so the UI order is stable.
    static func names() throws -> [String] {
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
    static func read(name: String) throws -> String? {
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
    static func write(name: String, value: String) throws {
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
    static func delete(name: String) throws {
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
    static func exportIntoEnvironment() throws {
        for name in try names() {
            guard let value = try read(name: name) else { continue }
            setenv(name, value, 1)
        }
    }

    /// A valid environment variable name: nonempty ASCII letters, digits,
    /// and underscores, not starting with a digit.
    static func isValidName(_ name: String) -> Bool {
        guard let first = name.unicodeScalars.first else { return false }
        guard CharacterSet(charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_")
            .contains(first) else { return false }
        let allowed = CharacterSet(
            charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_"
        )
        return name.unicodeScalars.allSatisfy { allowed.contains($0) }
    }
}
