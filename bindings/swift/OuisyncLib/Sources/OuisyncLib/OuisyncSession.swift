/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import Foundation
import MessagePack

public class OuisyncSession {
    // Used to send and receive messages from the Ouisync library
    let librarySender: OuisyncLibrarySenderProtocol

    var nextMessageId: MessageId = 0
    var pendingResponses: [MessageId: CheckedContinuation<Response, any Error>] = [:]
    var notificationSubscriptions: NotificationStream.State = NotificationStream.State()

    public init(_ libraryClient: OuisyncLibrarySenderProtocol) {
        self.librarySender = libraryClient
    }

    public func listRepositories() async throws -> [OuisyncRepository] {
        let response = try await sendRequest(MessageRequest.listRepositories());
        let handles = response.toUInt64Array()
        return handles.map({ OuisyncRepository($0, self) })
    }

    public func subscribeToRepositoryListChange() async throws -> NotificationStream {
        let subscriptionId = try await sendRequest(MessageRequest.subscribeToRepositoryListChange()).toUInt64();
        return NotificationStream(subscriptionId, notificationSubscriptions)
    }

    public func subscribeToRepositoryChange(_ repo: RepositoryHandle) async throws -> NotificationStream {
        let subscriptionId = try await sendRequest(MessageRequest.subscribeToRepositoryChange(repo)).toUInt64();
        return NotificationStream(subscriptionId, notificationSubscriptions)
    }

    internal func sendRequest(_ request: MessageRequest) async throws -> Response {
        let messageId = generateMessageId()

        async let onResponse = withCheckedThrowingContinuation { [weak self] continuation in
            guard let session = self else { return }

            synchronized(session) {
                session.pendingResponses[messageId] = continuation
                session.sendDataToOuisyncLib(Self.serialize(messageId, request));
            }
        }

        return try await onResponse
    }

    fileprivate static func serialize(_ messageId: MessageId, _ request: MessageRequest) -> [UInt8] {
        var message: [UInt8] = []
        message.append(contentsOf: withUnsafeBytes(of: messageId.bigEndian, Array.init))
        let payload = [MessagePackValue.string(request.functionName): request.functionArguments]
        message.append(contentsOf: pack(MessagePackValue.map(payload)))
        return message
    }

    fileprivate func generateMessageId() -> MessageId {
        synchronized(self) {
            let messageId = nextMessageId
            nextMessageId += 1
            return messageId
        }
    }

    fileprivate func sendDataToOuisyncLib(_ data: [UInt8]) {
        librarySender.sendDataToOuisyncLib(data);
    }

    // Use this function to pass data from the backend.
    // It may be called from a separate thread.
    public func onReceiveDataFromOuisyncLib(_ data: [UInt8]) {
        let maybe_message = IncomingMessage.deserialize(data)

        guard let message = maybe_message else {
            let hex = data.map({String(format:"%02x", $0)}).joined(separator: ",")
            // Likely cause is a version mismatch between the backend (Rust) and frontend (Swift) code.
            fatalError("Failed to parse incoming message from OuisyncLib [\(hex)]")
        }

        switch message.payload {
        case .response(let response):
            handleResponse(message.messageId, response)
        case .notification(let notification):
            handleNotification(message.messageId, notification)
        case .error(let error):
            handleError(message.messageId, error)
        }
    }

    fileprivate func handleResponse(_ messageId: MessageId, _ response: Response) {
        let maybePendingResponse = synchronized(self) { pendingResponses.removeValue(forKey: messageId) }

        guard let pendingResponse = maybePendingResponse else {
            fatalError("❗ Failed to match response to a request. messageId:\(messageId), repsponse:\(response) ")
        }

        pendingResponse.resume(returning: response)
    }

    fileprivate func handleNotification(_ messageId: MessageId, _ response: OuisyncNotification) {
        let maybeTx = synchronized(self) { notificationSubscriptions.registrations[messageId] }

        if let tx = maybeTx {
            tx.yield(())
        } else {
            NSLog("❗ Received unsolicited notification")
        }
    }

    fileprivate func handleError(_ messageId: MessageId, _ response: OuisyncError) {
        let maybePendingResponse = synchronized(self) { pendingResponses.removeValue(forKey: messageId) }

        guard let pendingResponse = maybePendingResponse else {
            fatalError("❗ Failed to match error response to a request. messageId:\(messageId), response:\(response)")
        }

        pendingResponse.resume(throwing: response)
    }

}

fileprivate func synchronized<T>(_ lock: AnyObject, _ closure: () throws -> T) rethrows -> T {
    objc_sync_enter(lock)
    defer { objc_sync_exit(lock) }
    return try closure()
}

public protocol OuisyncLibrarySenderProtocol {
    func sendDataToOuisyncLib(_ data: [UInt8])
}

public class NotificationStream {
    typealias Id = UInt64
    typealias Rx = AsyncStream<()>
    typealias RxIter = Rx.AsyncIterator
    typealias Tx = Rx.Continuation

    class State {
        var registrations: [Id: Tx] = [:]
    }

    let subscriptionId: Id
    let rx: Rx
    var rx_iter: RxIter
    var state: State

    init(_ subscriptionId: Id, _ state: State) {
        self.subscriptionId = subscriptionId
        var tx: Tx!
        rx = Rx (bufferingPolicy: Tx.BufferingPolicy.bufferingOldest(1), { tx = $0 })
        self.rx_iter = rx.makeAsyncIterator()

        self.state = state

        state.registrations[subscriptionId] = tx
    }

    public func next() async -> ()? {
        return await rx_iter.next()
    }

    deinit {
        // TODO: We should have a `close() async` function where we unsubscripbe
        // from the notifications.
        state.registrations.removeValue(forKey: subscriptionId)
    }
}

