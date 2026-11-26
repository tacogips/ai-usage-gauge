import CPTY
import Foundation
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

struct GeminiRateLimitFetcher: RateLimitLiveFetching {
  let environment: [String: String]

  func fetchLive(now: Date) async throws -> RateLimitObservation {
    guard let executable else { throw RateLimitErrorCode.cliMissing }
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    defer { try? FileManager.default.removeItem(at: directory) }

    // agy supplies a status line only in interactive mode. Start it on a private
    // terminal and read the documented JSON callback when account initialization finishes.
    let process = try launch(executable: executable, in: directory)
    defer { process.stop() }
    let statusFile = directory.appendingPathComponent("status.json")
    let clock = ContinuousClock()
    let deadline = clock.now + .seconds(12)
    while clock.now < deadline {
      try Task.checkCancellation()
      process.drain()
      if let data = try? Data(contentsOf: statusFile), let observation = try Self.observation(from: data, now: now) {
        await process.requestExit()
        return observation
      }
      guard process.isRunning else { throw RateLimitErrorCode.invalidResponse }
      try await Task.sleep(for: .milliseconds(100))
    }
    throw RateLimitErrorCode.timeout
  }

  static func observation(from data: Data, now: Date) throws -> RateLimitObservation? {
    let payload = try JSONDecoder().decode(StatusPayload.self, from: data)
    guard payload.product == "antigravity" else { throw RateLimitErrorCode.invalidResponse }
    let windows = try [("gemini-5h", 18_000), ("gemini-weekly", 604_800)].compactMap { key, seconds -> RateLimitWindow? in
      guard let bucket = payload.quota?[key], bucket.disabled != true,
            let remaining = bucket.remainingFraction, (0...1).contains(remaining) else { return nil }
      let reset = bucket.resetTime.flatMap(antigravityTimestamp)
        ?? bucket.resetInSeconds.flatMap { $0 > 0 ? now.addingTimeInterval($0) : nil }
      return try RateLimitWindow(windowSeconds: seconds, usedPercent: (1 - remaining) * 100, resetsAt: reset)
    }
    guard !windows.isEmpty else { return nil }
    return RateLimitObservation(planType: payload.planTier, observedAt: now, windows: windows)
  }

  private var executable: URL? {
    let home = environment["HOME"] ?? NSHomeDirectory()
    let path = (environment["PATH"] ?? "").split(separator: ":").map(String.init)
    return (path + ["\(home)/.local/bin", "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"])
      .filter { $0.hasPrefix("/") }
      .map { URL(fileURLWithPath: $0, isDirectory: true).appendingPathComponent("agy") }
      .first { FileManager.default.isExecutableFile(atPath: $0.path) }
  }

  private func launch(executable: URL, in directory: URL) throws -> AntigravityStatusProcess {
    let settings = ["statusLine": [
      "type": "command",
      "command": "umask 077; /bin/cat > status.tmp && /bin/mv -f status.tmp status.json"
    ]]
    let data = try JSONEncoder().encode(settings)
    guard let settingsArgument = String(bytes: data, encoding: .utf8) else { throw RateLimitErrorCode.invalidResponse }
    let process = Process()
    process.executableURL = executable
    process.arguments = ["--settings", settingsArgument]
    process.environment = environment.merging(["TERM": "xterm-256color"]) { _, terminal in terminal }
    process.currentDirectoryURL = directory
    return try AntigravityStatusProcess(process: process)
  }
}

private struct StatusPayload: Decodable {
  let product: String
  let planTier: String?
  let quota: [String: Bucket]?

  enum CodingKeys: String, CodingKey {
    case product, quota
    case planTier = "plan_tier"
  }
}

private struct Bucket: Decodable {
  let remainingFraction: Double?
  let resetTime: String?
  let resetInSeconds: Double?
  let disabled: Bool?

  enum CodingKeys: String, CodingKey {
    case remainingFraction = "remaining_fraction"
    case resetTime = "reset_time"
    case resetInSeconds = "reset_in_seconds"
    case disabled
  }
}

private struct AntigravityStatusProcess {
  private let process: Process
  private let controllerFD: Int32

  init(process: Process) throws {
    var controllerFD: Int32 = -1
    var terminalFD: Int32 = -1
    var size = CPTY.winsize(ws_row: 36, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0)
    guard CPTY.openpty(&controllerFD, &terminalFD, nil, nil, &size) == 0 else { throw RateLimitErrorCode.invalidResponse }
    let terminal = FileHandle(fileDescriptor: terminalFD, closeOnDealloc: true)
    process.standardInput = terminal
    process.standardOutput = terminal
    process.standardError = terminal
    do {
      guard fcntl(controllerFD, F_SETFL, O_NONBLOCK) != -1,
            fcntl(controllerFD, F_SETFD, FD_CLOEXEC) != -1 else { throw RateLimitErrorCode.invalidResponse }
      try process.run()
    } catch {
      close(controllerFD)
      throw error
    }
    try? terminal.close()
    self.process = process
    self.controllerFD = controllerFD
  }

  var isRunning: Bool { process.isRunning }

  func drain() {
    var buffer = [UInt8](repeating: 0, count: 4096)
    for _ in 0..<16 {
      guard read(controllerFD, &buffer, buffer.count) > 0 else { break }
    }
  }

  func requestExit() async {
    // Ctrl-D is agy's normal interactive exit. Allow a bounded cleanup before
    // the caller's deferred stop handles a stuck or unresponsive process.
    var endOfInput: UInt8 = 4
    _ = write(controllerFD, &endOfInput, 1)
    for _ in 0..<5 {
      guard isRunning, !Task.isCancelled else { return }
      drain()
      try? await Task.sleep(for: .milliseconds(50))
    }
  }

  func stop() {
    defer { close(controllerFD) }
    if process.isRunning { _ = kill(process.processIdentifier, SIGKILL) }
    process.waitUntilExit()
  }
}

func antigravityTimestamp(_ text: String) -> Date? {
  let formatter = ISO8601DateFormatter()
  formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
  if let date = formatter.date(from: text) { return date }
  formatter.formatOptions = [.withInternetDateTime]
  return formatter.date(from: text)
}
