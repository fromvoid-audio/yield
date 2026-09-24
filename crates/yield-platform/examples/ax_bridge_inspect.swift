// Inspect and optionally activate the OPT-1870 GPUI accessibility probe.
// Usage: swift ax_bridge_inspect.swift <pid> [press|focus]
import ApplicationServices
import Foundation

guard CommandLine.arguments.count >= 2, let pid = Int32(CommandLine.arguments[1]) else {
    fatalError("expected process id")
}

func attribute(_ element: AXUIElement, _ name: String) -> AnyObject? {
    var value: CFTypeRef?
    let status = AXUIElementCopyAttributeValue(element, name as CFString, &value)
    return status == .success ? value : nil
}

var match: AXUIElement?
var closeButton: AXUIElement?
var matchPosition: CGPoint?
var matchSize: CGSize?
func inspect(_ element: AXUIElement, depth: Int = 0) {
    guard depth < 12 else { return }
    let role = attribute(element, kAXRoleAttribute) ?? "?" as AnyObject
    let label = attribute(element, kAXDescriptionAttribute)
        ?? attribute(element, kAXTitleAttribute) ?? "?" as AnyObject
    let value = attribute(element, kAXValueAttribute) ?? "?" as AnyObject
    let subrole = attribute(element, kAXSubroleAttribute) ?? "?" as AnyObject
    print("\(String(repeating: "  ", count: depth))role=\(role) subrole=\(subrole) label=\(label) value=\(value)")
    if "\(subrole)" == "AXCloseButton" { closeButton = element }
    if "\(label)".contains("Probe toggle") {
        if let position = attribute(element, kAXPositionAttribute) {
            var point = CGPoint.zero
            if AXValueGetValue(position as! AXValue, .cgPoint, &point) {
                print("  position=\(point)")
                matchPosition = point
            }
        }
        if let size = attribute(element, kAXSizeAttribute) {
            var measured = CGSize.zero
            if AXValueGetValue(size as! AXValue, .cgSize, &measured) {
                print("  size=\(measured)")
                matchSize = measured
            }
        }
    }
    if "\(label)".contains("Probe toggle") { match = element }
    if let children = attribute(element, kAXChildrenAttribute) as? [AXUIElement] {
        for child in children { inspect(child, depth: depth + 1) }
    }
}

let app = AXUIElementCreateApplication(pid)
inspect(app)
if CommandLine.arguments.count >= 3, let button = match {
    let action = CommandLine.arguments[2]
    if action == "press" {
        print("press status=\(AXUIElementPerformAction(button, kAXPressAction as CFString).rawValue)")
    } else if action == "focus" {
        print("focus status=\(AXUIElementSetAttributeValue(button, kAXFocusedAttribute as CFString, kCFBooleanTrue).rawValue)")
    } else if action == "click", let origin = matchPosition, let size = matchSize {
        let center = CGPoint(x: origin.x + size.width / 2, y: origin.y + size.height / 2)
        CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: center, mouseButton: .left)?.post(tap: .cghidEventTap)
        Thread.sleep(forTimeInterval: 0.05)
        CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: center, mouseButton: .left)?.post(tap: .cghidEventTap)
    }
    Thread.sleep(forTimeInterval: 0.3)
    print("after value=\(attribute(button, kAXValueAttribute) ?? "?" as AnyObject)")
    print("after focused=\(attribute(button, kAXFocusedAttribute) ?? "?" as AnyObject)")
}
if CommandLine.arguments.count >= 3, CommandLine.arguments[2] == "close", let closeButton {
    print("close status=\(AXUIElementPerformAction(closeButton, kAXPressAction as CFString).rawValue)")
}
