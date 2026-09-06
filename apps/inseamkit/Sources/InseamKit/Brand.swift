import SwiftUI

/// The inseam brand tokens from `docs/brand/README.md`, as SwiftUI values so
/// every app shell draws the same seam. SwiftUI only — no AppKit or UIKit —
/// so this file compiles for macOS and iOS alike.
public enum Brand {
    /// Backgrounds. Near-black, faintly green.
    public static let groundHex: UInt32 = 0x0a0b0a
    /// The dashes, accents, links, highlights. Chartreuse.
    public static let threadHex: UInt32 = 0xd8ff1c
    /// The dots, body text. Off-white.
    public static let nodeHex: UInt32 = 0xf2f0e9

    public static let ground = Color(hex: groundHex)
    public static let thread = Color(hex: threadHex)
    public static let node = Color(hex: nodeHex)

    /// Monospace everywhere — headings, body, UI — so one helper stands in
    /// for `.system(style, design: .monospaced)` at every call site.
    public static func font(_ style: Font.TextStyle) -> Font {
        .system(style, design: .monospaced)
    }
}

extension Color {
    /// A color from a 24-bit `0xRRGGBB` value in sRGB, fully opaque.
    public init(hex: UInt32) {
        assert(hex <= 0xffffff)
        let red = Double((hex >> 16) & 0xff) / 255
        let green = Double((hex >> 8) & 0xff) / 255
        let blue = Double(hex & 0xff) / 255
        assert(red >= 0)
        assert(red <= 1)
        self.init(.sRGB, red: red, green: green, blue: blue, opacity: 1)
    }
}

/// The repeating `●▬▬●▬▬●` run from the brand's stitch strip, drawn at a
/// given height and as wide as its container allows. It always cuts on a
/// whole element, never mid-shape, so a strip is a whole number of nodes
/// and dashes. Decorative: hidden from accessibility.
public struct StitchStrip: View {
    /// Canonical units from the brand doc: node circle r 25, dash 49×35,
    /// gap 8 between every element, everything centered on one line.
    public static let nodeDiameter: CGFloat = 50
    public static let dashWidth: CGFloat = 49
    public static let dashHeight: CGFloat = 35
    public static let gap: CGFloat = 8
    /// The canonical line height is the tallest element.
    public static let lineHeight: CGFloat = nodeDiameter
    /// Bounds the drawing loop no matter how wide the container gets.
    public static let elementsMax: Int = 1_024

    public let height: CGFloat
    public let thread: Color
    public let node: Color

    public init(height: CGFloat, thread: Color = Brand.thread, node: Color = Brand.node) {
        assert(height > 0)
        self.height = height
        self.thread = thread
        self.node = node
    }

    /// Element `index` of the run: every third element is a node, the two
    /// between are dashes, starting and ending on a node.
    public static func elementIsNode(at index: Int) -> Bool {
        assert(index >= 0)
        return index % 3 == 0
    }

    /// Canonical width of element `index`.
    public static func elementWidth(at index: Int) -> CGFloat {
        elementIsNode(at: index) ? nodeDiameter : dashWidth
    }

    /// How many whole elements fit in `width` at `height`: the count the
    /// strip draws, so a strip never ends mid-shape.
    public static func elementsFitting(width: CGFloat, height: CGFloat) -> Int {
        assert(height > 0)
        let scale = height / lineHeight
        var x: CGFloat = 0
        var count = 0
        for index in 0..<elementsMax {
            let elementWidth = elementWidth(at: index) * scale
            guard x + elementWidth <= width else { break }
            x += elementWidth + gap * scale
            count += 1
        }
        assert(count <= elementsMax)
        return count
    }

    public var body: some View {
        Canvas { context, size in
            let scale = size.height / Self.lineHeight
            let count = Self.elementsFitting(width: size.width, height: size.height)
            var x: CGFloat = 0
            for index in 0..<count {
                let elementWidth = Self.elementWidth(at: index) * scale
                let rect = Self.elementRect(at: index, x: x, width: elementWidth, scale: scale)
                if Self.elementIsNode(at: index) {
                    context.fill(Path(ellipseIn: rect), with: .color(node))
                } else {
                    context.fill(Path(rect), with: .color(thread))
                }
                x += elementWidth + Self.gap * scale
            }
        }
        .frame(height: height)
        .accessibilityHidden(true)
    }

    /// The frame of one element, vertically centered on the line.
    private static func elementRect(
        at index: Int, x: CGFloat, width: CGFloat, scale: CGFloat
    ) -> CGRect {
        let elementHeight = (elementIsNode(at: index) ? nodeDiameter : dashHeight) * scale
        let y = (lineHeight * scale - elementHeight) / 2
        return CGRect(x: x, y: y, width: width, height: elementHeight)
    }
}
