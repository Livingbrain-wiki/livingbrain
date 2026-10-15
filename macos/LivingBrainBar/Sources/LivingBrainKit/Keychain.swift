import Foundation
import Security

/// Where an API token is kept. Implementations never write a file.
public protocol TokenStore: Sendable {
    func read() throws -> String?
    func write(_ token: String) throws
    func delete() throws
}

public enum KeychainError: Error, LocalizedError, Equatable {
    case status(OSStatus)

    public var errorDescription: String? {
        switch self {
        case let .status(code):
            let message = SecCopyErrorMessageString(code, nil) as String? ?? "error \(code)"
            return "Keychain: \(message)"
        }
    }
}

/// A generic-password keychain item.
public struct KeychainTokenStore: TokenStore {
    /// This app's own item.
    public static let appService = "wiki.livingbrain.mac"
    /// The CLI's item (crates/livingbrain-cli/src/auth.rs `SERVICE`); its
    /// account is the API base URL without a trailing slash.
    public static let cliService = "wiki.livingbrain.cli"

    public let service: String
    public let account: String

    public init(service: String, account: String) {
        self.service = service
        self.account = account
    }

    /// The account for a base URL, as the CLI's `user_key` spells it.
    public static func account(for baseURL: URL) -> String {
        var text = baseURL.absoluteString
        while text.hasSuffix("/") { text.removeLast() }
        return text
    }

    var query: [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }

    public func read() throws -> String? {
        var query = self.query
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = item as? Data else { throw KeychainError.status(status) }
        let token = String(decoding: data, as: UTF8.self)
        return token.isEmpty ? nil : token
    }

    public func write(_ token: String) throws {
        let data = Data(token.utf8)
        let update = SecItemUpdate(query as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if update == errSecSuccess { return }
        guard update == errSecItemNotFound else { throw KeychainError.status(update) }
        var add = query
        add[kSecValueData as String] = data
        add[kSecAttrLabel as String] = "Living Brain (\(account))"
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else { throw KeychainError.status(status) }
    }

    public func delete() throws {
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw KeychainError.status(status) }
    }
}

/// An in-memory store for tests and previews.
public final class MemoryTokenStore: TokenStore, @unchecked Sendable {
    private let lock = NSLock()
    private var token: String?

    public init(_ token: String? = nil) { self.token = token }

    public func read() throws -> String? { lock.lock(); defer { lock.unlock() }; return token }
    public func write(_ token: String) throws { lock.lock(); self.token = token; lock.unlock() }
    public func delete() throws { lock.lock(); token = nil; lock.unlock() }
}
