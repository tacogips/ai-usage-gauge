import AppCore
import AppKit

extension MenuBarDelegate {
  func usageIconsItem(selection: [PieChartID]) -> NSMenuItem {
    let root = NSMenuItem(title: "Usage icons", action: nil, keyEquivalent: "")
    let submenu = NSMenu()
    submenu.autoenablesItems = false
    for id in PieChartID.menuBarCases {
      let item = submenu.addItem(withTitle: RateLimitDisplayFormatting.title(for: id), action: #selector(toggleUsageIcon(_:)), keyEquivalent: "")
      let selected = selection.contains(id)
      item.target = self
      item.representedObject = id.rawValue
      item.state = selected ? .on : .off
      item.isEnabled = currentState != nil && !(selected && selection.count == 1)
    }
    root.submenu = submenu
    return root
  }

  @objc private func toggleUsageIcon(_ sender: NSMenuItem) {
    guard let name = sender.representedObject as? String, let id = PieChartID(rawValue: name) else { return }
    let enabled = sender.state != .on
    Task {
      guard await mutateState({ state in
        if state.menuBarPieCharts.contains(id) != enabled {
          state.menuBarPieCharts = PieChartSelection.toggled(id, in: state.menuBarPieCharts)
        }
      }) else { return }
      updateStatusTitle(budgetStatusText)
      (displaySettingsWindow?.contentView as? MenuBarDisplaySettingsView)?.update(state: currentState ?? AppState())
    }
  }

  @objc func showDisplaySettings() {
    if let window = displaySettingsWindow {
      (window.contentView as? MenuBarDisplaySettingsView)?.update(state: currentState ?? AppState())
      NSApplication.shared.activate(ignoringOtherApps: true)
      window.makeKeyAndOrderFront(nil)
      return
    }
    let view = MenuBarDisplaySettingsView(state: currentState ?? AppState())
    view.onChange = { [weak self, weak view] option, enabled in
      guard let self, let view else { return }
      Task {
        await self.mutateState { state in
          switch option {
          case .cost:
            state.menuBarShowCost = enabled
          case .icon(let id):
            if state.menuBarPieCharts.contains(id) != enabled {
              state.menuBarPieCharts = PieChartSelection.toggled(id, in: state.menuBarPieCharts)
            }
          case .ratio(let id):
            if id == .budget {
              state.menuBarCostOnly = !enabled
            }
            if state.menuBarRatioLabels.contains(id) != enabled {
              state.menuBarRatioLabels = RatioTextSelection.toggled(id, in: state.menuBarRatioLabels)
            }
          }
        }
        self.updateStatusTitle(self.budgetStatusText)
        view.update(state: self.currentState ?? AppState())
      }
    }
    let window = NSWindow(
      contentRect: view.bounds, styleMask: [.titled, .closable], backing: .buffered, defer: false
    )
    window.title = "Menu bar display"
    window.contentView = view
    window.isReleasedWhenClosed = false
    window.center()
    displaySettingsWindow = window
    NSApplication.shared.activate(ignoringOtherApps: true)
    window.makeKeyAndOrderFront(nil)
  }
}

@MainActor
final class MenuBarDisplaySettingsView: NSView {
  enum Option {
    case cost
    case icon(PieChartID)
    case ratio(PieChartID)
  }

  var onChange: ((Option, Bool) -> Void)?
  private var controls: [(option: Option, toggle: NSSwitch)] = []

  init(state: AppState) {
    super.init(frame: NSRect(x: 0, y: 0, width: 420, height: 640))
    let stack = NSStackView()
    stack.orientation = .vertical
    stack.alignment = .leading
    stack.spacing = 8
    stack.translatesAutoresizingMaskIntoConstraints = false
    addSubview(stack)
    stack.addArrangedSubview(NSTextField(wrappingLabelWithString: "Choose what appears in the menu bar. Changes are saved automatically."))
    addHeading("Usage icons", to: stack)
    for id in PieChartID.menuBarCases {
      addToggle(RateLimitDisplayFormatting.title(for: id), option: .icon(id), section: "Usage icons", to: stack)
    }
    let hint = NSTextField(labelWithString: "Keep at least one usage icon enabled.")
    hint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
    hint.textColor = .secondaryLabelColor
    stack.addArrangedSubview(hint)
    addHeading("Ratio text", to: stack)
    addToggle("Cost (USD)", option: .cost, section: "Ratio text", to: stack)
    for id in PieChartID.menuBarCases {
      let title = id == .budget ? "Budget percentage" : RateLimitDisplayFormatting.title(for: id)
      addToggle(title, option: .ratio(id), section: "Ratio text", to: stack)
    }
    NSLayoutConstraint.activate([
      stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 24),
      stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -24),
      stack.topAnchor.constraint(equalTo: topAnchor, constant: 24),
      stack.bottomAnchor.constraint(lessThanOrEqualTo: bottomAnchor, constant: -24)
    ])
    update(state: state)
  }

  @available(*, unavailable)
  required init?(coder: NSCoder) {
    fatalError("init(coder:) has not been implemented")
  }

  func update(state: AppState) {
    for (option, toggle) in controls {
      let selected: Bool
      switch option {
      case .cost: selected = state.menuBarShowCost
      case .icon(let id): selected = state.menuBarPieCharts.contains(id)
      case .ratio(let id):
        selected = state.menuBarRatioLabels.contains(id) && (id != .budget || !state.menuBarCostOnly)
      }
      toggle.state = selected ? .on : .off
      if case .icon = option {
        toggle.isEnabled = !(selected && state.menuBarPieCharts.count == 1)
      } else {
        toggle.isEnabled = true
      }
    }
  }

  private func addHeading(_ title: String, to stack: NSStackView) {
    let label = NSTextField(labelWithString: title)
    label.font = .boldSystemFont(ofSize: NSFont.systemFontSize)
    stack.setCustomSpacing(16, after: stack.arrangedSubviews.last ?? label)
    stack.addArrangedSubview(label)
  }

  private func addToggle(_ title: String, option: Option, section: String, to stack: NSStackView) {
    let toggle = NSSwitch()
    toggle.target = self
    toggle.action = #selector(change(_:))
    toggle.tag = controls.count
    toggle.setAccessibilityLabel("\(section): \(title)")
    controls.append((option, toggle))
    let row = NSStackView(views: [NSTextField(labelWithString: title), NSView(), toggle])
    row.orientation = .horizontal
    stack.addArrangedSubview(row)
    row.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
  }

  @objc private func change(_ sender: NSSwitch) {
    let option = controls[sender.tag].option
    controls.forEach { $0.toggle.isEnabled = false }
    onChange?(option, sender.state == .on)
  }
}
