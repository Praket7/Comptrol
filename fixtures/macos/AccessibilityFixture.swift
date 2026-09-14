import Cocoa

final class FixtureDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private var button: NSButton!

    func applicationDidFinishLaunching(_ notification: Notification) {
        let content = NSView(frame: NSRect(x: 0, y: 0, width: 360, height: 180))
        window = NSWindow(
            contentRect: content.frame,
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Comptrol AX Fixture"
        window.contentView = content
        button = NSButton(title: "Submit", target: self, action: #selector(submit))
        button.frame = NSRect(x: 120, y: 70, width: 120, height: 36)
        content.addSubview(button)
        window.orderFrontRegardless()
    }

    @objc private func submit() {
        button.title = "Submitted"
    }
}

let application = NSApplication.shared
let delegate = FixtureDelegate()
application.delegate = delegate
application.setActivationPolicy(.accessory)
application.run()
