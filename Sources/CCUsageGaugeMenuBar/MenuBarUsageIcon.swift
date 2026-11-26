import AppCore
import AppKit
import CoreImage

@MainActor
enum MenuBarUsageIcon {
  private struct Artwork {
    let solid: NSImage
    let outline: NSImage
    /// Cumulative fraction of hollow-interior pixels from bottom (`index 0`) to top,
    /// used to map a usage fraction to an area-proportional fill height so narrow
    /// tips (such as the Gemini star) do not hide small usage percentages inside the stroke.
    let cumulativeInteriorByRow: [Double]

    func fillHeightFraction(for fraction: Double) -> CGFloat {
      guard fraction > 0 else { return 0 }
      guard fraction < 1 else { return 1 }
      let count = cumulativeInteriorByRow.count
      guard count > 0 else { return CGFloat(fraction) }
      var previous = 0.0
      for (index, cumulative) in cumulativeInteriorByRow.enumerated() {
        if cumulative >= fraction {
          let span = cumulative - previous
          let withinRow = span > 0 ? (fraction - previous) / span : 0
          return CGFloat((Double(index) + withinRow) / Double(count))
        }
        previous = cumulative
      }
      return 1
    }
  }

  private static let context = CIContext(options: [.useSoftwareRenderer: true])
  private static let claude = artwork(MenuBarProviderIcons.claude)
  private static let codex = artwork(MenuBarProviderIcons.chatGPT)

  private static let gemini = artwork(NSImage(size: NSSize(width: 36, height: 36), flipped: false) { _ in
    let mark = NSBezierPath()
    mark.move(to: NSPoint(x: 18, y: 35))
    mark.curve(to: NSPoint(x: 35, y: 18), controlPoint1: NSPoint(x: 21, y: 25), controlPoint2: NSPoint(x: 25, y: 21))
    mark.curve(to: NSPoint(x: 18, y: 1), controlPoint1: NSPoint(x: 25, y: 15), controlPoint2: NSPoint(x: 21, y: 11))
    mark.curve(to: NSPoint(x: 1, y: 18), controlPoint1: NSPoint(x: 15, y: 11), controlPoint2: NSPoint(x: 11, y: 15))
    mark.curve(to: NSPoint(x: 18, y: 35), controlPoint1: NSPoint(x: 11, y: 21), controlPoint2: NSPoint(x: 15, y: 25))
    mark.close()
    NSColor.black.setFill()
    mark.fill()
    return true
  })

  static func image(chart: PieChartID, fraction: Double?) -> NSImage {
    let artwork = switch chart.provider {
    case .claude: claude
    case .gemini: gemini
    case .codex, nil: codex
    }
    let normalized = fraction.map { $0.isFinite ? min(max($0, 0), 1) : 0 }
    let fillHeight = normalized.map { artwork.fillHeightFraction(for: $0) }
    let image = NSImage(size: NSSize(width: 18, height: 18), flipped: false) { bounds in
      artwork.outline.draw(in: bounds)
      if let fillHeight {
        // AppKit's unflipped coordinates start at the bottom of the icon.
        NSGraphicsContext.saveGraphicsState()
        NSBezierPath(rect: NSRect(x: bounds.minX, y: bounds.minY, width: bounds.width, height: bounds.height * fillHeight)).addClip()
        artwork.solid.draw(in: bounds)
        NSGraphicsContext.restoreGraphicsState()
      } else {
        let slash = NSBezierPath()
        slash.move(to: NSPoint(x: bounds.minX + 2, y: bounds.minY + 2))
        slash.line(to: NSPoint(x: bounds.maxX - 2, y: bounds.maxY - 2))
        slash.lineWidth = 1
        NSColor.black.setStroke()
        slash.stroke()
      }
      return true
    }
    image.isTemplate = true
    image.accessibilityDescription = normalized.map { "\(RateLimitDisplayFormatting.title(for: chart)) \(Int(($0 * 100).rounded()))% used" }
      ?? "\(RateLimitDisplayFormatting.title(for: chart)) unavailable"
    return image
  }

  private static func artwork(_ source: NSImage) -> Artwork {
    guard let original = source.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
      preconditionFailure("Invalid provider artwork")
    }
    let bitmap = NSBitmapImageRep(cgImage: original)
    var minX = bitmap.pixelsWide
    var minY = bitmap.pixelsHigh
    var maxX = 0
    var maxY = 0
    for y in 0..<bitmap.pixelsHigh {
      for x in 0..<bitmap.pixelsWide where (bitmap.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.02 {
        minX = min(minX, x); maxX = max(maxX, x)
        minY = min(minY, y); maxY = max(maxY, y)
      }
    }
    guard minX <= maxX, minY <= maxY,
          let cgImage = original.cropping(to: CGRect(x: minX, y: minY, width: maxX - minX + 1, height: maxY - minY + 1)) else {
      preconditionFailure("Empty provider artwork")
    }
    let size = NSSize(width: 18, height: 18)
    let solid = NSImage(cgImage: cgImage, size: size)
    let input = CIImage(cgImage: cgImage)
    // Subtract an eroded mask to retain the actual mark's outer and inner edges.
    let eroded = input.applyingFilter("CIMorphologyMinimum", parameters: [kCIInputRadiusKey: max(1, CGFloat(cgImage.width) / 18 * 0.5)])
    guard let innerMask = context.createCGImage(eroded, from: input.extent) else {
      preconditionFailure("Could not outline provider artwork")
    }
    let interior = NSImage(cgImage: innerMask, size: size)
    let outline = NSImage(size: size, flipped: false) { bounds in
      solid.draw(in: bounds)
      interior.draw(in: bounds, from: .zero, operation: .destinationOut, fraction: 1)
      return true
    }
    return Artwork(
      solid: solid,
      outline: outline,
      cumulativeInteriorByRow: cumulativeInteriorProfile(innerMask)
    )
  }

  /// Counts opaque interior pixels per row from bottom to top and returns the
  /// normalized cumulative distribution (0...1).
  private static func cumulativeInteriorProfile(_ mask: CGImage) -> [Double] {
    let bitmap = NSBitmapImageRep(cgImage: mask)
    let height = bitmap.pixelsHigh
    let width = bitmap.pixelsWide
    guard height > 0, width > 0 else { return [] }
    var rowCounts = [Double](repeating: 0, count: height)
    var total = 0.0
    // NSBitmapImageRep y = 0 is the top row; reverse so index 0 is the bottom of the icon.
    for y in 0..<height {
      var count = 0.0
      for x in 0..<width where (bitmap.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.1 {
        count += 1
      }
      rowCounts[height - 1 - y] = count
      total += count
    }
    guard total > 0 else { return [] }
    var running = 0.0
    return rowCounts.map { count in
      running += count
      return running / total
    }
  }
}
