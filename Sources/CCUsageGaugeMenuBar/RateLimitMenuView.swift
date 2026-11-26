import AppCore
import AppKit
import Foundation

enum RateLimitMenuView {
  @MainActor static func items(selection: [PieChartID], response: RateLimitsResponse?, now: Date) -> [NSMenuItem] {
    PieChartID.allCases.filter { $0 != .budget && selection.contains($0) }.map { chart in
      let title = RateLimitDisplayFormatting.compactMenuLine(for: chart, response: response, now: now)
      let detail = RateLimitDisplayFormatting.menuLine(for: chart, response: response, now: now)
      return MenuInformationItem.make(title, detail: detail)
    }
  }
}
