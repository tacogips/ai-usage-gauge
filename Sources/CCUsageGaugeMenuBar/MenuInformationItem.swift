import AppKit

@MainActor
enum MenuInformationItem {
  static let maximumTextWidth: CGFloat = 204

  static func make(_ title: String, detail: String? = nil) -> NSMenuItem {
    let label = NSTextField(wrappingLabelWithString: title)
    label.font = .menuFont(ofSize: 0)
    label.textColor = .secondaryLabelColor
    label.lineBreakMode = .byCharWrapping
    label.setAccessibilityLabel(detail ?? title)

    let bounds = NSRect(x: 0, y: 0, width: maximumTextWidth, height: 1_000)
    let size = label.cell?.cellSize(forBounds: bounds) ?? label.fittingSize
    let horizontalInset: CGFloat = 14
    let verticalInset: CGFloat = 3
    let width = min(maximumTextWidth, ceil(size.width))
    let height = ceil(size.height)
    label.frame = NSRect(x: horizontalInset, y: verticalInset, width: width, height: height)
    let container = NSView(frame: NSRect(
      x: 0, y: 0, width: width + horizontalInset * 2, height: height + verticalInset * 2
    ))
    container.addSubview(label)

    let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
    item.view = container
    item.toolTip = detail
    item.isEnabled = false
    return item
  }
}
