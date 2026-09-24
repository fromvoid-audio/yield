// Native AX contract check for OPT-1878's adjustable slider probe.
// Usage: swift ax_slider_inspect.swift <pid>
import ApplicationServices
import Foundation

guard CommandLine.arguments.count == 2, let pid = Int32(CommandLine.arguments[1]) else {
    fatalError("expected probe pid")
}
let app = AXUIElementCreateApplication(pid)
func attribute(_ element: AXUIElement, _ key: String) -> AnyObject? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, key as CFString, &value) == .success ? value : nil
}
func find(_ element: AXUIElement, _ id: String, depth: Int = 0) -> AXUIElement? {
    if depth > 10 { return nil }
    if attribute(element, kAXIdentifierAttribute) as? String == id { return element }
    for child in (attribute(element, kAXChildrenAttribute) as? [AXUIElement]) ?? [] {
        if let found = find(child, id, depth: depth + 1) { return found }
    }
    return nil
}
func actions(_ element: AXUIElement) -> [String] {
    var names: CFArray?
    guard AXUIElementCopyActionNames(element, &names) == .success else { return [] }
    return names as? [String] ?? []
}
func number(_ element: AXUIElement, _ key: String) -> Double? {
    (attribute(element, key) as? NSNumber)?.doubleValue
}
func snapshot(_ element: AXUIElement) -> String {
    let role = attribute(element, kAXRoleAttribute) as? String ?? "?"
    let value = number(element, kAXValueAttribute)
    let min = number(element, kAXMinValueAttribute)
    let max = number(element, kAXMaxValueAttribute)
    let enabled = attribute(element, kAXEnabledAttribute) as? NSNumber
    var settable = DarwinBoolean(false)
    let setStatus = AXUIElementIsAttributeSettable(element, kAXValueAttribute as CFString, &settable)
    return "role=\(role) value=\(String(describing: value)) min=\(String(describing: min)) max=\(String(describing: max)) enabled=\(String(describing: enabled)) settable=\(setStatus.rawValue):\(settable.boolValue) actions=\(actions(element))"
}
func check(_ condition: Bool, _ description: String) {
    guard condition else { print("FAIL: \(description)"); fflush(stdout); exit(1) }
    print("PASS: \(description)")
}
func settle() { Thread.sleep(forTimeInterval: 0.25) }
guard let slider = find(app, "slider-probe-volume"),
      let toggle = find(app, "slider-probe-toggle"),
      let remove = find(app, "slider-probe-remove") else { fatalError("probe controls missing") }
print("initial " + snapshot(slider))
check(attribute(slider, kAXRoleAttribute) as? String == "AXSlider", "native slider role")
check(number(slider, kAXValueAttribute) == 0.5, "numeric initial value")
check(number(slider, kAXMinValueAttribute) == 0 && number(slider, kAXMaxValueAttribute) == 1, "numeric bounds")
check(Set(actions(slider)) == Set(["AXIncrement", "AXDecrement"]), "increment and decrement actions")
var settable = DarwinBoolean(false)
check(AXUIElementIsAttributeSettable(slider, kAXValueAttribute as CFString, &settable) == .success && settable.boolValue, "value settable")
check(AXUIElementSetAttributeValue(slider, kAXValueAttribute as CFString, NSNumber(value: 0.3)) == .success, "native numeric set request")
settle()
check(number(slider, kAXValueAttribute) == 0.3, "set applied by GPUI owner")
check(AXUIElementPerformAction(slider, kAXIncrementAction as CFString) == .success, "native increment request")
settle()
check(abs((number(slider, kAXValueAttribute) ?? 0) - 0.4) < 0.00001, "increment applied by GPUI owner")
check(AXUIElementPerformAction(slider, kAXDecrementAction as CFString) == .success, "native decrement request")
settle()
check(abs((number(slider, kAXValueAttribute) ?? 0) - 0.3) < 0.00001, "decrement applied by GPUI owner")
_ = AXUIElementSetAttributeValue(slider, kAXValueAttribute as CFString, NSNumber(value: 1.2))
settle()
print("after invalid " + snapshot(slider))
check(abs((number(slider, kAXValueAttribute) ?? 0) - 0.3) < 0.00001, "out-of-range value rejected")
check(AXUIElementPerformAction(toggle, kAXPressAction as CFString) == .success, "disable slider")
settle()
print("disabled " + snapshot(slider))
check((attribute(slider, kAXEnabledAttribute) as? NSNumber)?.boolValue == false, "disabled state")
check(actions(slider).isEmpty, "disabled slider has no actions")
settable = DarwinBoolean(true)
check(AXUIElementIsAttributeSettable(slider, kAXValueAttribute as CFString, &settable) == .success && !settable.boolValue, "disabled slider value read-only")
check(AXUIElementPerformAction(remove, kAXPressAction as CFString) == .success, "retire slider")
settle()
check(find(app, "slider-probe-volume") == nil, "retired slider absent from window")
check(actions(slider).isEmpty, "retired slider object has no actions")
settable = DarwinBoolean(true)
let retiredStatus = AXUIElementIsAttributeSettable(slider, kAXValueAttribute as CFString, &settable)
print("retired value setter status=\(retiredStatus.rawValue) settable=\(settable.boolValue)")
check(retiredStatus != .success || !settable.boolValue, "retired slider cannot set value")
print("PASS: slider native contract")
