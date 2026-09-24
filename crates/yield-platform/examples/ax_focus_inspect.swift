// Native focus inspector for ax_registry_probe. Run with its process ID.
import ApplicationServices
import Foundation

guard CommandLine.arguments.count == 2, let pid = Int32(CommandLine.arguments[1]) else {
    fatalError("usage: swift ax_focus_inspect.swift <probe-pid>")
}

let app = AXUIElementCreateApplication(pid)
func attribute(_ element: AXUIElement, _ name: String) -> AnyObject? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}
func find(_ element: AXUIElement, id: String, depth: Int = 0) -> AXUIElement? {
    guard depth < 12 else { return nil }
    if attribute(element, kAXIdentifierAttribute) as? String == id { return element }
    for child in attribute(element, kAXChildrenAttribute) as? [AXUIElement] ?? [] {
        if let found = find(child, id: id, depth: depth + 1) { return found }
    }
    return nil
}
func focusedID() -> String? {
    guard let focused = attribute(app, kAXFocusedUIElementAttribute) as! AXUIElement? else {
        return nil
    }
    return attribute(focused, kAXIdentifierAttribute) as? String
}
func settle() { Thread.sleep(forTimeInterval: 0.35) }
func focus(_ element: AXUIElement) {
    precondition(AXUIElementSetAttributeValue(element, kAXFocusedAttribute as CFString, kCFBooleanTrue) == .success)
    settle()
}
func press(_ element: AXUIElement) {
    precondition(AXUIElementPerformAction(element, kAXPressAction as CFString) == .success)
    settle()
}

var notifications: [String] = []
let callback: AXObserverCallback = { _, element, _, _ in
    if let id = attribute(element, kAXIdentifierAttribute) as? String {
        notifications.append(id)
    }
}
var observer: AXObserver?
precondition(AXObserverCreate(pid, callback, &observer) == .success)
guard let observer,
      let first = find(app, id: "first-toggle"),
      let second = find(app, id: "second-control") else {
    fatalError("probe children unavailable")
}
for element in [app, first, second] {
    precondition(AXObserverAddNotification(observer, element, kAXFocusedUIElementChangedNotification as CFString, nil) == .success)
}
CFRunLoopAddSource(CFRunLoopGetCurrent(), AXObserverGetRunLoopSource(observer), .defaultMode)

focus(second)
precondition(focusedID() != "second-control", "disabled child gained focus")
focus(first)
precondition(focusedID() == "first-toggle", "first child is not application AX focus")
CFRunLoopRunInMode(.defaultMode, 0.2, false)
press(first)
focus(second)
precondition(focusedID() == "second-control", "second child is not application AX focus")
CFRunLoopRunInMode(.defaultMode, 0.2, false)
precondition(notifications.contains("first-toggle") && notifications.contains("second-control"), "native focus notifications missing")
press(second) // Reorder the live children.
focus(second)
precondition(focusedID() == "second-control", "reordered child lost AX focus")
press(second) // Retire the second child.
precondition(find(app, id: "second-control") == nil, "retired child remains in tree")
precondition(focusedID() != "second-control", "retired child remains application AX focus")
precondition(AXUIElementSetAttributeValue(second, kAXFocusedAttribute as CFString, kCFBooleanTrue) != .success, "held retired element accepted focus")
print("PASS: disabled, live, reordered, and retired focus; application focused child; native notifications")
