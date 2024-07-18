/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import Foundation
import System
import MessagePack

public class OuisyncRepository: Hashable, CustomDebugStringConvertible {
    let session: OuisyncSession
    public let handle: RepositoryHandle

    public init(_ handle: RepositoryHandle, _ session: OuisyncSession) {
        self.handle = handle
        self.session = session
    }

    public func getName() async throws -> String {
        let data = try await session.sendRequest(.getRepositoryName(handle)).toData()
        return String(decoding: data, as: UTF8.self)
    }

    public func fileEntry(_ path: FilePath) -> OuisyncFileEntry {
        OuisyncFileEntry(path, self)
    }

    public func getEntryVersionHash(_ path: FilePath) async throws -> Data {
        try await session.sendRequest(.getEntryVersionHash(handle, path)).toData()
    }

    public func directoryEntry(_ path: FilePath) -> OuisyncDirectoryEntry {
        OuisyncDirectoryEntry(path, self)
    }

    public func getRootDirectory() -> OuisyncDirectoryEntry {
        return OuisyncDirectoryEntry(FilePath("/"), self)
    }

    public func createFile(_ path: FilePath) async throws -> OuisyncFile {
        let handle = try await session.sendRequest(.fileCreate(handle, path)).toUInt64()
        return OuisyncFile(handle, self)
    }

    public func openFile(_ path: FilePath) async throws -> OuisyncFile {
        let handle = try await session.sendRequest(.fileOpen(handle, path)).toUInt64()
        return OuisyncFile(handle, self)
    }

    public func deleteFile(_ path: FilePath) async throws {
        let _ = try await session.sendRequest(.fileRemove(handle, path))
    }

    public func createDirectory(_ path: FilePath) async throws {
        let _ = try await session.sendRequest(.directoryCreate(handle, path))
    }

    public func deleteDirectory(_ path: FilePath, recursive: Bool) async throws {
        let _ = try await session.sendRequest(.directoryRemove(handle, path, recursive))
    }

    public func moveEntry(_ sourcePath: FilePath, _ destinationPath: FilePath) async throws {
        let _ = try await session.sendRequest(.repositoryMoveEntry(handle, sourcePath, destinationPath))
    }

    public static func == (lhs: OuisyncRepository, rhs: OuisyncRepository) -> Bool {
        return lhs.session === rhs.session && lhs.handle == rhs.handle
    }

    public func hash(into hasher: inout Hasher) {
        hasher.combine(ObjectIdentifier(session))
        hasher.combine(handle)
    }

    public var debugDescription: String {
        return "OuisyncRepository(handle: \(handle))"
    }
}
