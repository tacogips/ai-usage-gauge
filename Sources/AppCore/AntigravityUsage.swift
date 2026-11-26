import CSQLite
import Foundation

/// Reads usage metadata from the Antigravity CLI's local conversation exports.
public actor AntigravityUsageLoader {
  private struct CachedTranscript {
    let file: UsageEventLogFile
    let steps: [Step]
  }

  private let root: URL
  private var cache: [URL: CachedTranscript] = [:]
  private var workspaces: [String: String] = [:]

  public init(root: URL) { self.root = root }

  public static func production(environment: [String: String] = ProcessInfo.processInfo.environment) -> AntigravityUsageLoader {
    let home = URL(fileURLWithPath: environment["HOME"] ?? NSHomeDirectory(), isDirectory: true)
    return AntigravityUsageLoader(root: home.appendingPathComponent(".gemini/antigravity-cli", isDirectory: true))
  }

  func usage(from start: Date, through end: Date, calendar: Calendar, machine: String) throws -> CCUsageDetailedUsage {
    guard start <= end, let exclusiveEnd = calendar.date(byAdding: .day, value: 1, to: end) else {
      return CCUsageDetailedUsage(metrics: [], sessions: [])
    }
    if let indexed = Self.workspaces(in: root.appendingPathComponent("conversation_summaries.db")) {
      workspaces = indexed
    }
    let sessions = try transcripts().flatMap { transcript -> [CCUsageSessionMetricRecord] in
      let conversation = transcript.file.url.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().lastPathComponent
      return transcript.steps.compactMap { step in
        guard step.timestamp >= start, step.timestamp < exclusiveEnd else { return nil }
        return step.record(machine: machine, directory: workspaces[conversation])
      }
    }
    return CCUsageDetailedUsage(
      metrics: coalescingSameKeyMetrics(sessions.map { row in
        CCUsageMetricRecord(
          date: calendarDayString(row.timestamp, calendar: calendar),
          agent: row.agent, model: row.model, costUSD: row.costUSD,
          inputTokens: row.inputTokens, outputTokens: row.outputTokens,
          cacheCreationTokens: row.cacheCreationTokens, cacheReadTokens: row.cacheReadTokens,
          machine: machine, directory: row.directory, effort: row.effort
        )
      }),
      sessions: coalescingSameKeySessions(sessions)
    )
  }

  private func transcripts() throws -> [CachedTranscript] {
    let brain = root.appendingPathComponent("brain", isDirectory: true)
    let manager = FileManager.default
    let keys: Set<URLResourceKey> = [.isRegularFileKey, .contentModificationDateKey, .fileSizeKey]
    let conversations = (try? manager.contentsOfDirectory(at: brain, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles])) ?? []
    var updated: [URL: CachedTranscript] = [:]
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    for conversation in conversations {
      try Task.checkCancellation()
      guard UUID(uuidString: conversation.lastPathComponent) != nil else { continue }
      // Both transcript variants contain the same usage steps.
      let logs = conversation.appendingPathComponent(".system_generated/logs", isDirectory: true)
      let transcript = ["transcript.jsonl", "transcript_full.jsonl"].lazy.compactMap { name -> (URL, URLResourceValues)? in
        let url = logs.appendingPathComponent(name)
        guard let values = try? url.resourceValues(forKeys: keys), values.isRegularFile == true else { return nil }
        return (url, values)
      }.first
      guard let (url, values) = transcript else { continue }
      let file = UsageEventLogFile(url: url, size: UInt64(max(0, values.fileSize ?? 0)), modified: values.contentModificationDate)
      if let existing = cache[url], existing.file == file {
        updated[url] = existing
        continue
      }
      var steps: [Int: Step] = [:]
      do {
        try UsageEventLogReader.forEachLine(in: url, to: file.size, matchingAny: [Data(#""input_tokens""#.utf8), Data(#""output_tokens""#.utf8)]) { line in
          if let row = try? decoder.decode(TranscriptRow.self, from: line), let step = row.step {
            steps[row.stepIndex] = step
          }
        }
        updated[url] = CachedTranscript(file: file, steps: Array(steps.values))
      } catch {
        updated[url] = cache[url]
      }
    }
    cache = updated
    return Array(updated.values)
  }

  private static func workspaces(in url: URL) -> [String: String]? {
    if let indexed = readWorkspaces(path: url.path, flags: SQLITE_OPEN_READONLY) { return indexed }
    // A closed WAL database may have no sidecars. Read its checkpointed state
    // without creating sidecars, but only when no active WAL is present.
    guard !FileManager.default.fileExists(atPath: url.path + "-wal") else { return nil }
    return readWorkspaces(path: url.absoluteString + "?immutable=1", flags: SQLITE_OPEN_READONLY | SQLITE_OPEN_URI)
  }

  private static func readWorkspaces(path: String, flags: Int32) -> [String: String]? {
    var database: OpaquePointer?
    defer { sqlite3_close(database) }
    guard sqlite3_open_v2(path, &database, flags, nil) == SQLITE_OK else { return nil }
    sqlite3_busy_timeout(database, 200)
    var statement: OpaquePointer?
    defer { sqlite3_finalize(statement) }
    guard sqlite3_prepare_v2(database, "SELECT conversation_id, workspace_uris FROM conversation_summaries", -1, &statement, nil) == SQLITE_OK else {
      return nil
    }
    var result: [String: String] = [:]
    let decoder = JSONDecoder()
    var code = sqlite3_step(statement)
    while code == SQLITE_ROW {
      defer { code = sqlite3_step(statement) }
      guard let id = sqlite3_column_text(statement, 0), let raw = sqlite3_column_text(statement, 1),
            let locations = try? decoder.decode([String].self, from: Data(String(cString: raw).utf8)),
            let location = locations.first, let workspace = URL(string: location), workspace.isFileURL,
            workspace.path.hasPrefix("/") else { continue }
      result[String(cString: id)] = workspace.standardizedFileURL.path
    }
    return code == SQLITE_DONE ? result : nil
  }

  private struct TranscriptRow: Decodable {
    let stepIndex: Int
    let source: String
    let createdAt: String
    let model: String
    let inputTokens: Int?
    let outputTokens: Int?
    let cacheReadTokens: Int?
    let cacheCreationTokens: Int?

    var step: Step? {
      guard source == "MODEL", !model.isEmpty, let timestamp = antigravityTimestamp(createdAt) else { return nil }
      let counts = [inputTokens ?? 0, outputTokens ?? 0, cacheReadTokens ?? 0, cacheCreationTokens ?? 0]
      guard counts.contains(where: { $0 > 0 }), counts.allSatisfy({ (0...Int(Int32.max)).contains($0) }) else { return nil }
      return Step(timestamp: timestamp, identifier: model, input: counts[0], output: counts[1], cacheRead: counts[2], cacheCreation: counts[3])
    }
  }

  private struct Step {
    let timestamp: Date
    let identifier: String
    let input: Int
    let output: Int
    let cacheRead: Int
    let cacheCreation: Int

    func record(machine: String, directory: String?) -> CCUsageSessionMetricRecord {
      let effort = ["xhigh", "high", "medium", "low", "max", "minimal"].first { identifier.hasSuffix("-" + $0) }
      let model = effort.map { String(identifier.dropLast($0.count + 1)) } ?? identifier
      let cost = AntigravityModelPricing.cost(model: model, date: timestamp, input: input, output: output, cacheRead: cacheRead)
      return CCUsageSessionMetricRecord(
        timestamp: timestamp, agent: "agy", model: model, costUSD: cost,
        inputTokens: input, outputTokens: output, cacheCreationTokens: cacheCreation, cacheReadTokens: cacheRead,
        dataQuality: .timestamped, machine: machine, directory: directory, effort: effort
      )
    }
  }
}

private enum AntigravityModelPricing {
  private struct Rates {
    let input: Decimal
    let output: Decimal
    let cacheRead: Decimal

    init(_ input: Decimal, _ output: Decimal, _ cacheRead: Decimal) {
      self.input = input
      self.output = output
      self.cacheRead = cacheRead
    }
  }

  // Standard text rates per million tokens from https://ai.google.dev/gemini-api/docs/pricing.
  // Keep unknown models in token history without guessing a price.
  static func cost(model: String, date: Date, input: Int, output: Int, cacheRead: Int) -> Decimal {
    let prices: Rates
    switch model {
    case "gemini-4-neon":
      prices = Rates(3, 15, 0.15)
    case "gemini-4-hydrogen":
      prices = Rates(1.5, 7.5, 0.075)
    case "gemini-4-argon", "gemini-4-carbon":
      prices = date < Date(timeIntervalSince1970: 1_796_083_200) ? Rates(2, 10, 0.1) : Rates(4, 20, 0.2) // 2026-12-01 UTC
    case "gemini-3.8-flash":
      prices = date < Date(timeIntervalSince1970: 1_798_761_600) ? Rates(0.75, 3.75, 0.075) : Rates(1.5, 7.5, 0.15) // 2027-01-01 UTC
    default:
      return .zero
    }
    return (Decimal(input) * prices.input + Decimal(output) * prices.output + Decimal(cacheRead) * prices.cacheRead) / 1_000_000
  }
}
