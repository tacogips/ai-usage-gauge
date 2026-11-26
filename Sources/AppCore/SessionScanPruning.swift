import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// Helpers for deriving `--since`-based modification-time cutoffs and building
/// optional pruned session mirrors (used by remote adapters and explicit callers).
public enum SessionScanPruning {
  /// Buffer below the `--since` day (parsed as UTC midnight) that keeps the
  /// cutoff safe for any `--timezone` the query is evaluated in (max UTC
  /// offset is 14h) and for the remote adapter's `touch -t`, which applies
  /// the token in the remote machine's local timezone.
  static let cutoffBufferSeconds: TimeInterval = 48 * 3_600

  /// Buffer used when `--timezone` is explicitly specified on a local query.
  /// `blocks` uses 6h so a 5-hour billing block spanning midnight is still
  /// included; `daily`/`session` use 1h for clock-skew safety.
  static let localDailyCutoffBufferSeconds: TimeInterval = 3_600
  static let localBlocksCutoffBufferSeconds: TimeInterval = 6 * 3_600

  /// Absolute cutoff instant derived from a `--since yyyy-MM-dd` argument,
  /// or nil when the query is unbounded and pruning must not happen.
  public static func mtimeCutoff(arguments: [String]) -> Date? {
    guard let sinceIndex = arguments.firstIndex(of: "--since"),
          arguments.index(after: sinceIndex) < arguments.endIndex else { return nil }
    let sinceValue = arguments[arguments.index(after: sinceIndex)]
    if let tzIndex = arguments.firstIndex(of: "--timezone"),
       arguments.index(after: tzIndex) < arguments.endIndex,
       let timeZone = TimeZone(identifier: arguments[arguments.index(after: tzIndex)]) {
      let formatter = DateFormatter()
      formatter.locale = Locale(identifier: "en_US_POSIX")
      formatter.timeZone = timeZone
      formatter.dateFormat = "yyyy-MM-dd"
      if let day = formatter.date(from: sinceValue) {
        let buffer = arguments.first == "blocks"
          ? localBlocksCutoffBufferSeconds
          : localDailyCutoffBufferSeconds
        return day.addingTimeInterval(-buffer)
      }
    }
    return utcMtimeCutoff(sinceValue: sinceValue)
  }

  /// `touch -t` token (CCYYMMDDhhmm) for the SSH source adapter. Formatted in
  /// UTC; the 48-hour cutoff buffer absorbs the remote machine's local-time reading.
  public static func mtimeCutoffToken(arguments: [String]) -> String? {
    guard let sinceIndex = arguments.firstIndex(of: "--since"),
          arguments.index(after: sinceIndex) < arguments.endIndex,
          let cutoff = utcMtimeCutoff(sinceValue: arguments[arguments.index(after: sinceIndex)]) else { return nil }
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(identifier: "UTC")
    formatter.dateFormat = "yyyyMMddHHmm"
    return formatter.string(from: cutoff)
  }

  private static func utcMtimeCutoff(sinceValue: String) -> Date? {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(identifier: "UTC")
    formatter.dateFormat = "yyyy-MM-dd"
    guard let day = formatter.date(from: sinceValue) else { return nil }
    return day.addingTimeInterval(-cutoffBufferSeconds)
  }

  /// Builds a mirror of `rootPath` whose scan directory contains only the
  /// JSONL files modified at or after `cutoff`, preserving relative layout so
  /// path-derived metadata stays intact. Files are hard-linked (copied when
  /// linking fails, e.g. across volumes). Returns nil when the mirror cannot
  /// be built. The caller removes the returned directory once finished.
  public static func prunedRoot(
    original rootPath: String,
    agent: MachineSessionAgent,
    cutoff: Date,
    fileManager: FileManager = .default
  ) -> URL? {
    let scanRoot = URL(fileURLWithPath: rootPath, isDirectory: true)
      .appendingPathComponent(agent.scanDirectoryName, isDirectory: true)
    var isDirectory: ObjCBool = false
    guard fileManager.fileExists(atPath: scanRoot.path, isDirectory: &isDirectory),
          isDirectory.boolValue else { return nil }
    let keys: [URLResourceKey] = [.isRegularFileKey, .contentModificationDateKey]
    let keySet = Set(keys)
    guard let enumerator = fileManager.enumerator(
      at: scanRoot,
      includingPropertiesForKeys: keys,
      options: [.skipsPackageDescendants, .skipsHiddenFiles]
    ) else { return nil }
    let mirrorBase = (try? fileManager.url(
      for: .itemReplacementDirectory,
      in: .userDomainMask,
      appropriateFor: scanRoot,
      create: true
    )) ?? fileManager.temporaryDirectory
    let mirrorRoot = mirrorBase.appendingPathComponent(
      "ccusage-gauge-pruned-\(agent.rawValue)-\(UUID().uuidString)",
      isDirectory: true
    )
    let mirrorScan = mirrorRoot.appendingPathComponent(agent.scanDirectoryName, isDirectory: true)
    do {
      try fileManager.createDirectory(at: mirrorScan, withIntermediateDirectories: true)
      let rawPrefix = scanRoot.path.hasSuffix("/") ? scanRoot.path : scanRoot.path + "/"
      let standardizedPrefix = scanRoot.standardizedFileURL.path.hasSuffix("/")
        ? scanRoot.standardizedFileURL.path
        : scanRoot.standardizedFileURL.path + "/"
      var createdDirs: Set<String> = [mirrorScan.path]
      let mirrorScanPath = mirrorScan.path
      for case let url as URL in enumerator {
        guard url.pathExtension == "jsonl",
              let values = try? url.resourceValues(forKeys: keySet),
              values.isRegularFile == true,
              let modified = values.contentModificationDate,
              modified >= cutoff else { continue }
        let sourcePath = url.path
        let relativePath: String
        if sourcePath.hasPrefix(rawPrefix) {
          relativePath = String(sourcePath.dropFirst(rawPrefix.count))
        } else if sourcePath.hasPrefix(standardizedPrefix) {
          relativePath = String(sourcePath.dropFirst(standardizedPrefix.count))
        } else {
          let standardized = url.standardizedFileURL.path
          guard standardized.hasPrefix(standardizedPrefix) else { continue }
          relativePath = String(standardized.dropFirst(standardizedPrefix.count))
        }
        guard !relativePath.isEmpty else { continue }
        let destinationPath = mirrorScanPath + "/" + relativePath
        if let slash = destinationPath.lastIndex(of: "/") {
          let parentPath = String(destinationPath[..<slash])
          if createdDirs.insert(parentPath).inserted {
            try fileManager.createDirectory(atPath: parentPath, withIntermediateDirectories: true)
          }
        }
        if link(sourcePath, destinationPath) != 0 {
          try fileManager.copyItem(atPath: sourcePath, toPath: destinationPath)
        }
      }
      return mirrorRoot
    } catch {
      try? fileManager.removeItem(at: mirrorRoot)
      return nil
    }
  }
}
