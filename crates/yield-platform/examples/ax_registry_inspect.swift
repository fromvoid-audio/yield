// Native AX inspector for OPT-1872's two-control GPUI fixture.
// Usage: swift ax_registry_inspect.swift <pid> [press|focus|click|close] [control-id]
import ApplicationServices
import AppKit
import Foundation

guard CommandLine.arguments.count >= 2, let pid = Int32(CommandLine.arguments[1]) else {
    fatalError("expected process id")
}

func attribute(_ element: AXUIElement, _ name: String) -> AnyObject? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}

func point(_ element: AXUIElement) -> CGPoint? {
    guard let value = attribute(element, kAXPositionAttribute) else { return nil }
    var point = CGPoint.zero
    return AXValueGetValue(value as! AXValue, .cgPoint, &point) ? point : nil
}

func size(_ element: AXUIElement) -> CGSize? {
    guard let value = attribute(element, kAXSizeAttribute) else { return nil }
    var size = CGSize.zero
    return AXValueGetValue(value as! AXValue, .cgSize, &size) ? size : nil
}

var controls: [String: AXUIElement] = [:]
var closeButton: AXUIElement?
func inspect(_ element: AXUIElement, depth: Int = 0) {
    guard depth < 12 else { return }
    let role = attribute(element, kAXRoleAttribute) ?? "?" as AnyObject
    let subrole = attribute(element, kAXSubroleAttribute) ?? "?" as AnyObject
    let id = attribute(element, kAXIdentifierAttribute) ?? "?" as AnyObject
    let label = attribute(element, kAXDescriptionAttribute)
        ?? attribute(element, kAXTitleAttribute) ?? "?" as AnyObject
    let value = attribute(element, kAXValueAttribute) ?? "?" as AnyObject
    let enabled = attribute(element, kAXEnabledAttribute) ?? "?" as AnyObject
    let focused = attribute(element, kAXFocusedAttribute) ?? "?" as AnyObject
    print("\(String(repeating: "  ", count: depth))role=\(role) id=\(id) label=\(label) value=\(value) enabled=\(enabled) focused=\(focused)")
    if "\(subrole)" == "AXCloseButton" { closeButton = element }
    if "\(id)" == "first-toggle" || "\(id)" == "second-control" {
        controls["\(id)"] = element
        print("  frame=\(String(describing: point(element))) \(String(describing: size(element)))")
    }
    if let children = attribute(element, kAXChildrenAttribute) as? [AXUIElement] {
        for child in children { inspect(child, depth: depth + 1) }
    }
}

let app = AXUIElementCreateApplication(pid)
inspect(app)
if CommandLine.arguments.count >= 3 {
    let action = CommandLine.arguments[2]
    if action == "close", let closeButton {
        print("close status=\(AXUIElementPerformAction(closeButton, kAXPressAction as CFString).rawValue)")
    } else if CommandLine.arguments.count >= 4,
              let element = controls[CommandLine.arguments[3]] {
        if action == "press" {
            print("press status=\(AXUIElementPerformAction(element, kAXPressAction as CFString).rawValue)")
        } else if action == "focus" {
            print("focus status=\(AXUIElementSetAttributeValue(element, kAXFocusedAttribute as CFString, kCFBooleanTrue).rawValue)")
        } else if action == "click", let origin = point(element), let frame = size(element) {
            _ = NSRunningApplication(processIdentifier: pid)?.activate()
            Thread.sleep(forTimeInterval: 0.1)
            let center = CGPoint(x: origin.x + frame.width / 2, y: origin.y + frame.height / 2)
            CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: center, mouseButton: .left)?.post(tap: .cghidEventTap)
            Thread.sleep(forTimeInterval: 0.05)
            CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: center, mouseButton: .left)?.post(tap: .cghidEventTap)
        }
        Thread.sleep(forTimeInterval: 0.3)
        print("after:")
        controls.removeAll()
        inspect(app)
    }
}
