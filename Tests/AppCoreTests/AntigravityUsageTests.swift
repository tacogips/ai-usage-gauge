import CSQLite
import Foundation
import Testing
@testable import AppCore

@Suite struct AntigravityUsageTests {
  @Test func readsGeminiWindowsFromAntigravityStatus() throws {
    let now = Date(timeIntervalSince1970: 1_795_000_000)
    let payload = Data("""
      {"product":"antigravity","plan_tier":"Pro","quota":{
        "gemini-5h":{"remaining_fraction":0.75,"reset_in_seconds":120},
        "gemini-weekly":{"remaining_fraction":0.5,"reset_time":"2026-11-28T03:00:00Z"},
        "3p-weekly":{"remaining_fraction":0.02}
      }}
      """.utf8)
    let observation = try #require(try GeminiRateLimitFetcher.observation(from: payload, now: now))
    #expect(observation.planType == "Pro")
    #expect(observation.windows.map(\.kind) == [.fiveHour, .weekly])
    #expect(observation.windows.map(\.usedPercent) == [25, 50])
    #expect(observation.windows.first?.resetsAt == now.addingTimeInterval(120))
    #expect(observation.windows.last?.resetsAt?.ISO8601Format() == "2026-11-28T03:00:00Z")
    let disabled = Data(#"{"product":"antigravity","quota":{"gemini-5h":{"remaining_fraction":1,"disabled":true}}}"#.utf8)
    #expect(try GeminiRateLimitFetcher.observation(from: disabled, now: now) == nil)
  }

  @Test func canReadStatusFromAChildWithATerminal() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: root) }
    let cli = root.appendingPathComponent("agy")
    let script = """
      #!/bin/sh
      test "$#" -eq 2 && test "$1" = "--settings" && test -t 0 && test -t 1 || exit 2
      printf '%s' '{"product":"antigravity","quota":{"gemini-weekly":{"remaining_fraction":0.75}}}' > status.json
      exec /bin/sleep 15
      """
    try Data(script.utf8).write(to: cli)
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: cli.path)
    let fetcher = GeminiRateLimitFetcher(environment: ["PATH": root.path, "HOME": root.path])
    let observation = try await fetcher.fetchLive(now: Date())
    #expect(observation.windows.map(\.usedPercent) == [25])
  }

  @Test func addsTimestampedUsageToSnapshotsWithoutCountingTranscriptCopies() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let conversation = "12345678-1234-1234-1234-123456789012"
    let logs = root.appendingPathComponent("brain/\(conversation)/.system_generated/logs")
    try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
    let data = Data("""
      {"step_index":1,"source":"MODEL","created_at":"2026-11-26T01:00:00Z","model":"gemini-4-neon-high","input_tokens":1000,"output_tokens":100,"cache_read_tokens":200}
      {"step_index":2,"source":"MODEL","created_at":"2026-11-25T23:59:59Z","model":"gemini-4-neon-low","input_tokens":25}
      {"step_index":3,"source":"USER","created_at":"2026-11-26T09:01:00Z","content":"ignored"}
      {"step_index":4,"source":"MODEL","created_at":"2026-11-26T01:02:00Z","model":"unknown-model","input_tokens":20}
      {"step_index":5,
      """.utf8)
    try data.write(to: logs.appendingPathComponent("transcript.jsonl"))
    try data.write(to: logs.appendingPathComponent("transcript_full.jsonl"))
    try createWorkspace(at: root.appendingPathComponent("conversation_summaries.db"), conversation: conversation)
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = try #require(TimeZone(identifier: "UTC"))
    let now = try #require(antigravityTimestamp("2026-11-26T01:04:00Z"))
    let service = SnapshotService(
      stateStore: StateStore(fileURL: root.appendingPathComponent("state.json")),
      client: CCUsageClient(commandRunner: EmptyUsageRunner(), machine: "local"),
      calculator: ResetWindowCalculator(calendar: calendar),
      antigravityUsageLoader: AntigravityUsageLoader(root: root)
    )
    let snapshot = try await service.snapshot(now: now, earliestDate: now)
    #expect(snapshot.dashboardMetrics.map(\.totalTokens).reduce(0, +) == 1320)
    #expect(snapshot.dashboardMetrics.allSatisfy { $0.agent == "agy" && $0.directory == "/projects/a fixture" })
    #expect(snapshot.dashboardSessions.count == 2)
    #expect(snapshot.dashboardSessions.first?.model == "gemini-4-neon")
    #expect(snapshot.dashboardSessions.first?.effort == "high")
    #expect(snapshot.dashboardSessions.first?.dataQuality == .timestamped)
    #expect(snapshot.costSinceResetUSD == Decimal(453) / 100_000)
    #expect(snapshot.points.count == 1)
    #expect(try await service.menuBarSnapshot(now: now).costSinceResetUSD == snapshot.costSinceResetUSD)
  }

  private func createWorkspace(at url: URL, conversation: String) throws {
    var database: OpaquePointer?
    defer { sqlite3_close(database) }
    try #require(sqlite3_open(url.path, &database) == SQLITE_OK)
    let sql = """
      PRAGMA journal_mode=WAL;
      CREATE TABLE conversation_summaries (conversation_id TEXT, workspace_uris TEXT);
      INSERT INTO conversation_summaries VALUES ('\(conversation)', '["file:/projects/a%20fixture"]');
      """
    try #require(sqlite3_exec(database, sql, nil, nil, nil) == SQLITE_OK)
  }
}

private struct EmptyUsageRunner: CCUsageCommandRunner {
  func run(arguments: [String], timeoutSeconds: TimeInterval) async throws -> ProcessResult {
    ProcessResult(stdout: Data(#"{"blocks":[],"daily":[],"session":[]}"#.utf8), stderr: Data(), exitStatus: 0)
  }
}
