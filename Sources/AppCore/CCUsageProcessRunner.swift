import Foundation
@preconcurrency import Dispatch
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

public enum ProcessTerminationReason: Equatable, Sendable {
  case exit
  case uncaughtSignal
}

public struct ProcessResult: Equatable, Sendable {
  public let stdout: Data
  public let stderr: Data
  public let exitStatus: Int32
  public let terminationReason: ProcessTerminationReason

  public init(
    stdout: Data,
    stderr: Data,
    exitStatus: Int32,
    terminationReason: ProcessTerminationReason = .exit
  ) {
    self.stdout = stdout
    self.stderr = stderr
    self.exitStatus = exitStatus
    self.terminationReason = terminationReason
  }
}

public enum ProcessExecutionFailure: Error, Equatable, Sendable {
  case spawnFailed
  case timedOut
}

public protocol CCUsageProcessRunning: Sendable {
  func run(executable: URL, arguments: [String], timeoutSeconds: TimeInterval) async throws -> ProcessResult
}

public protocol CCUsageEnvironmentProcessRunning: CCUsageProcessRunning {
  func run(
    executable: URL,
    arguments: [String],
    environment: [String: String],
    timeoutSeconds: TimeInterval
  ) async throws -> ProcessResult
}

public struct CCUsageProcessRunner: CCUsageEnvironmentProcessRunning, Sendable {
  private static let pipeIOQueue = DispatchQueue(
    label: "com.tacogips.ccusage-gauge.process-pipe-io",
    qos: .utility,
    attributes: .concurrent
  )

  private final class TerminationSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var terminated = false
    private var waiters: [UUID: CheckedContinuation<Bool, Never>] = [:]

    func markTerminated() {
      lock.lock()
      terminated = true
      let pending = Array(waiters.values)
      waiters.removeAll()
      lock.unlock()
      for continuation in pending {
        continuation.resume(returning: true)
      }
    }

    func wait() async -> Bool {
      let id = UUID()
      return await withTaskCancellationHandler {
        await withCheckedContinuation { continuation in
          lock.lock()
          if terminated {
            lock.unlock()
            continuation.resume(returning: true)
            return
          }
          if Task.isCancelled {
            lock.unlock()
            continuation.resume(returning: false)
            return
          }
          waiters[id] = continuation
          lock.unlock()
        }
      } onCancel: {
        lock.lock()
        let continuation = waiters.removeValue(forKey: id)
        lock.unlock()
        continuation?.resume(returning: false)
      }
    }
  }

  public init() {}

  public func run(
    executable: URL,
    arguments: [String],
    timeoutSeconds: TimeInterval = 30
  ) async throws -> ProcessResult {
    try await runProcess(executable: executable, arguments: arguments, environment: nil, timeoutSeconds: timeoutSeconds)
  }

  public func run(
    executable: URL,
    arguments: [String],
    environment: [String: String],
    timeoutSeconds: TimeInterval
  ) async throws -> ProcessResult {
    try await runProcess(executable: executable, arguments: arguments, environment: environment, timeoutSeconds: timeoutSeconds)
  }

  private func runProcess(
    executable: URL,
    arguments: [String],
    environment: [String: String]?,
    timeoutSeconds: TimeInterval
  ) async throws -> ProcessResult {
    let worker = Task.detached(priority: .utility) {
      let process = Process()
      let stdout = Pipe()
      let stderr = Pipe()
      let termination = TerminationSignal()
      process.executableURL = executable
      process.arguments = arguments
      process.environment = environment
      process.standardOutput = stdout
      process.standardError = stderr
      process.terminationHandler = { _ in
        termination.markTerminated()
      }
      #if canImport(Glibc)
      do { try Self.launchWithUnblockedSignals(process) } catch { throw ProcessExecutionFailure.spawnFailed }
      #else
      do { try process.run() } catch { throw ProcessExecutionFailure.spawnFailed }
      #endif
      let processID = process.processIdentifier
      let ownsProcessGroup = setpgid(processID, processID) == 0

      let outHandle = stdout.fileHandleForReading
      let errHandle = stderr.fileHandleForReading
      let outDescriptor = dup(outHandle.fileDescriptor)
      let errDescriptor = dup(errHandle.fileDescriptor)
      guard outDescriptor >= 0, errDescriptor >= 0 else {
        if outDescriptor >= 0 { close(outDescriptor) }
        if errDescriptor >= 0 { close(errDescriptor) }
        signalProcess(processID, signal: SIGKILL, processGroup: ownsProcessGroup)
        throw ProcessExecutionFailure.spawnFailed
      }
      let outTask = Task.detached(priority: .utility) { await readPipe(outDescriptor) }
      let errTask = Task.detached(priority: .utility) { await readPipe(errDescriptor) }

      // Event-driven wait: wakes immediately on process exit, timeout, or cancellation
      // with zero polling intervals.
      _ = await waitForExit(termination: termination, timeoutSeconds: max(0, timeoutSeconds))
      let wasCancelled = Task.isCancelled
      if process.isRunning {
        signalProcess(processID, signal: SIGTERM, processGroup: ownsProcessGroup)
        _ = await waitForExit(termination: termination, timeoutSeconds: 0.25)
        if process.isRunning {
          signalProcess(processID, signal: SIGKILL, processGroup: ownsProcessGroup)
        }
        // `Process.waitUntilExit()` can remain blocked on macOS even after the
        // child has disappeared. A timeout must stay bounded so the collector
        // can publish the failure and retry on its next polling interval.
        outTask.cancel()
        errTask.cancel()
        if wasCancelled { throw CancellationError() }
        throw ProcessExecutionFailure.timedOut
      }
      if wasCancelled {
        outTask.cancel()
        errTask.cancel()
        throw CancellationError()
      }
      return ProcessResult(
        stdout: await outTask.value,
        stderr: await errTask.value,
        exitStatus: process.terminationStatus,
        terminationReason: process.terminationReason == .uncaughtSignal ? .uncaughtSignal : .exit
      )
    }
    return try await withTaskCancellationHandler {
      try await worker.value
    } onCancel: {
      worker.cancel()
    }
  }

  private func waitForExit(termination: TerminationSignal, timeoutSeconds: TimeInterval) async -> Bool {
    guard timeoutSeconds > 0 else { return false }
    return await withTaskGroup(of: Bool.self) { group in
      group.addTask { await termination.wait() }
      group.addTask {
        do {
          try await Task.sleep(for: .seconds(timeoutSeconds))
        } catch {
          // Cancelled because the process exited or parent task was cancelled.
        }
        return false
      }
      let exited = await group.next() ?? false
      group.cancelAll()
      return exited
    }
  }

  #if canImport(Glibc)
  /// Foundation's Linux spawn inherits this worker thread's blocked signals.
  /// In particular, blocked SIGCHLD leaves a shell waiting forever for its children.
  /// Keep mask changes on this thread with no suspension and restore them on error.
  private static func launchWithUnblockedSignals(_ process: Process) throws {
    var previous = sigset_t()
    var unblocked = sigset_t()
    sigemptyset(&unblocked)
    guard pthread_sigmask(SIG_SETMASK, &unblocked, &previous) == 0 else {
      throw ProcessExecutionFailure.spawnFailed
    }
    defer { pthread_sigmask(SIG_SETMASK, &previous, nil) }
    try process.run()
  }
  #endif

  #if canImport(Darwin)
  private final class PipeReadState: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    private var finished = false
    private var continuation: CheckedContinuation<Data, Never>?
    private let source: DispatchSourceRead
    private let descriptor: Int32

    init(descriptor: Int32, source: DispatchSourceRead) {
      self.descriptor = descriptor
      self.source = source
    }

    func install(_ continuation: CheckedContinuation<Data, Never>) {
      lock.lock()
      if finished {
        let result = data
        lock.unlock()
        continuation.resume(returning: result)
        return
      }
      self.continuation = continuation
      lock.unlock()
    }

    func drainAvailable() {
      var chunk = [UInt8](repeating: 0, count: 64 * 1_024)
      while true {
        let count = chunk.withUnsafeMutableBytes { bytes in
          read(descriptor, bytes.baseAddress, bytes.count)
        }
        if count > 0 {
          lock.lock()
          if !finished {
            data.append(contentsOf: chunk.prefix(count))
          }
          lock.unlock()
        } else if count == 0 {
          finish()
          return
        } else if errno == EINTR {
          continue
        } else if errno == EAGAIN || errno == EWOULDBLOCK {
          return
        } else {
          finish()
          return
        }
      }
    }

    func finish() {
      lock.lock()
      guard !finished else {
        lock.unlock()
        return
      }
      finished = true
      let result = data
      let pending = continuation
      continuation = nil
      lock.unlock()
      source.cancel()
      pending?.resume(returning: result)
    }
  }

  private func readPipe(_ descriptor: Int32) async -> Data {
    let flags = fcntl(descriptor, F_GETFL)
    guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else {
      close(descriptor)
      return Data()
    }
    if Task.isCancelled {
      close(descriptor)
      return Data()
    }
    let source = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: Self.pipeIOQueue)
    let state = PipeReadState(descriptor: descriptor, source: source)
    source.setEventHandler {
      state.drainAvailable()
    }
    source.setCancelHandler {
      close(descriptor)
    }
    return await withTaskCancellationHandler {
      await withCheckedContinuation { continuation in
        state.install(continuation)
        if Task.isCancelled {
          state.finish()
          return
        }
        source.resume()
      }
    } onCancel: {
      state.finish()
    }
  }
  #else
  private func readPipe(_ descriptor: Int32) async -> Data {
    defer { close(descriptor) }
    let flags = fcntl(descriptor, F_GETFL)
    guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == 0 else { return Data() }
    var result = Data()
    var buffer = [UInt8](repeating: 0, count: 64 * 1_024)
    while !Task.isCancelled {
      let count = buffer.withUnsafeMutableBytes { bytes in
        read(descriptor, bytes.baseAddress, bytes.count)
      }
      if count > 0 {
        result.append(contentsOf: buffer.prefix(count))
      } else if count == 0 {
        break
      } else if errno == EINTR {
        continue
      } else if errno == EAGAIN || errno == EWOULDBLOCK {
        try? await Task.sleep(for: .milliseconds(10))
      } else {
        break
      }
    }
    return result
  }
  #endif

  private func signalProcess(
    _ processID: Int32,
    signal: Int32,
    processGroup: Bool
  ) {
    if processGroup {
      _ = kill(-processID, signal)
    }
    _ = kill(processID, signal)
  }
}
