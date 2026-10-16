import Foundation

/// Streams one file to disk with progress. An ephemeral session, so nothing
/// is cached, and a non-200 answer is an error rather than a saved page.
enum UpdateDownloader {
    static func download(
        _ url: URL,
        to destination: URL,
        request: URLRequest,
        progress: @escaping @Sendable (Double) -> Void
    ) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let delegate = Delegate(destination: destination, progress: progress, continuation: continuation)
            let session = URLSession(configuration: .ephemeral, delegate: delegate, delegateQueue: nil)
            delegate.session = session
            session.downloadTask(with: request).resume()
        }
    }

    /// Delegate callbacks run on one serial queue, so the mutable state here
    /// is only touched from that queue.
    private final class Delegate: NSObject, URLSessionDownloadDelegate, @unchecked Sendable {
        let destination: URL
        let progress: @Sendable (Double) -> Void
        var session: URLSession?
        private var continuation: CheckedContinuation<Void, Error>?
        private var failure: Error?

        init(destination: URL, progress: @escaping @Sendable (Double) -> Void,
             continuation: CheckedContinuation<Void, Error>) {
            self.destination = destination
            self.progress = progress
            self.continuation = continuation
        }

        func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask,
                        didWriteData bytesWritten: Int64, totalBytesWritten: Int64,
                        totalBytesExpectedToWrite: Int64) {
            guard totalBytesExpectedToWrite > 0 else { return }
            progress(min(1, Double(totalBytesWritten) / Double(totalBytesExpectedToWrite)))
        }

        func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL) {
            if let http = downloadTask.response as? HTTPURLResponse, http.statusCode != 200 {
                failure = UpdateError.server(http.statusCode)
                return
            }
            do {
                try? FileManager.default.removeItem(at: destination)
                try FileManager.default.moveItem(at: location, to: destination)
            } catch {
                failure = error
            }
        }

        func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
            self.session?.finishTasksAndInvalidate()
            self.session = nil
            let outcome = error ?? failure
            let waiting = continuation
            continuation = nil
            if let outcome {
                waiting?.resume(throwing: outcome)
            } else {
                waiting?.resume()
            }
        }
    }
}
