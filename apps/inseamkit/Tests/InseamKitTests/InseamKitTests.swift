// Swift Testing rather than XCTest: the Xcode Command Line Tools — the only
// toolchain the app build requires — ship Testing.framework but not XCTest,
// and the library's tests must run on that same minimal setup.
import SwiftUI
import Testing

import InseamKit

#if canImport(AppKit)
import AppKit
#endif

@Suite struct PluginUploadTests {
    @Test func acceptsLowercaseId() {
        #expect(PluginUpload.pluginIdProblem("ocr") == nil)
        #expect(PluginUpload.pluginIdProblem("my-plugin_2") == nil)
    }

    @Test func rejectsUppercaseId() {
        #expect(PluginUpload.pluginIdProblem("OCR") != nil)
    }

    @Test func rejectsEmptyId() {
        #expect(PluginUpload.pluginIdProblem("") != nil)
    }

    @Test(arguments: ["-ocr", "_ocr"])
    func rejectsIdStartingWithPunctuation(id: String) {
        #expect(PluginUpload.pluginIdProblem(id) != nil)
    }

    @Test func rejectsIdLongerThanLimit() {
        let atLimit = String(repeating: "a", count: Int(PLUGIN_ID_CHARS_MAX))
        let overLimit = atLimit + "a"
        #expect(PluginUpload.pluginIdProblem(atLimit) == nil)
        #expect(PluginUpload.pluginIdProblem(overLimit) != nil)
    }
}

@Suite struct BrandTests {
    @Test func hexTokensMatchBrandDoc() {
        #expect(Brand.groundHex == 0x0a0b0a)
        #expect(Brand.threadHex == 0xd8ff1c)
        #expect(Brand.nodeHex == 0xf2f0e9)
    }

    @Test func colorsAreBuiltFromTheirHexTokens() {
        #expect(Brand.ground == Color(hex: 0x0a0b0a))
        #expect(Brand.thread == Color(hex: 0xd8ff1c))
        #expect(Brand.node == Color(hex: 0xf2f0e9))
        #expect(Brand.ground != Brand.node)
    }

    #if canImport(AppKit)
    /// Read the channels back through AppKit on macOS to pin the hex → sRGB
    /// conversion to the documented values, within half an 8-bit step.
    @Test(arguments: [
        (Brand.ground, UInt32(0x0a0b0a)),
        (Brand.thread, UInt32(0xd8ff1c)),
        (Brand.node, UInt32(0xf2f0e9)),
    ])
    func colorChannelsDecodeHex(color: Color, hex: UInt32) throws {
        let native = try #require(NSColor(color).usingColorSpace(.sRGB))
        let tolerance = 0.5 / 255
        let red = Double((hex >> 16) & 0xff) / 255
        let green = Double((hex >> 8) & 0xff) / 255
        let blue = Double(hex & 0xff) / 255
        #expect(abs(native.redComponent - red) <= tolerance)
        #expect(abs(native.greenComponent - green) <= tolerance)
        #expect(abs(native.blueComponent - blue) <= tolerance)
        #expect(native.alphaComponent == 1)
    }
    #endif
}

@Suite struct StitchStripTests {
    @Test func elementsAlternateNodeDashDash() {
        #expect(StitchStrip.elementIsNode(at: 0))
        #expect(!StitchStrip.elementIsNode(at: 1))
        #expect(!StitchStrip.elementIsNode(at: 2))
        #expect(StitchStrip.elementIsNode(at: 3))
    }

    /// `assets/stitch.svg` is seven elements in a 394×50 box; the strip must
    /// fit exactly those seven at canonical scale and no more.
    @Test func canonicalStripFitsSevenElements() {
        #expect(StitchStrip.elementsFitting(width: 394, height: 50) == 7)
        #expect(StitchStrip.elementsFitting(width: 393, height: 50) == 6)
        #expect(StitchStrip.elementsFitting(width: 197, height: 25) == 7)
    }

    @Test func stripNeverCutsMidElement() {
        // Wide enough for the first node and gap but not the first dash.
        #expect(StitchStrip.elementsFitting(width: 100, height: 50) == 1)
        // Narrower than one node: nothing is drawn rather than a sliver.
        #expect(StitchStrip.elementsFitting(width: 49, height: 50) == 0)
    }

    @Test func fitIsBoundedByElementsMax() {
        let count = StitchStrip.elementsFitting(width: 1_000_000, height: 1)
        #expect(count == StitchStrip.elementsMax)
    }
}
