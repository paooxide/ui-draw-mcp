// Text recognition for agentctl, via Apple's Vision framework.
//
// Compiled on first use by `mcp-macos/src/ocr.rs` rather than at build time:
// `cargo build` must not depend on the Xcode Command Line Tools being present,
// and a machine without them should get a clear UNSUPPORTED_OS at call time
// instead of a build that fails.
//
// Usage:  agentctl-ocr <png-path> [--lang en-US,fr-FR] [--fast]
// Output: {"width":W,"height":H,"lines":[{"text":..,"confidence":..,
//          "box":{"x":..,"y":..,"w":..,"h":..}}]}
//
// Boxes are in image pixels with a **top-left** origin. Vision reports
// normalised coordinates from the bottom left, so both the flip and the scale
// happen here — the Rust side should never have to know about Vision's
// conventions.

import Foundation
import CoreGraphics
import ImageIO
import Vision

func fail(_ code: Int32, _ message: String) -> Never {
    FileHandle.standardError.write(("agentctl-ocr: " + message + "\n").data(using: .utf8)!)
    exit(code)
}

var args = Array(CommandLine.arguments.dropFirst())
var languages: [String] = []
var fast = false
var path: String? = nil

var i = 0
while i < args.count {
    switch args[i] {
    case "--lang":
        i += 1
        if i < args.count { languages = args[i].split(separator: ",").map(String.init) }
    case "--fast":
        fast = true
    default:
        path = args[i]
    }
    i += 1
}

guard let path else { fail(2, "usage: agentctl-ocr <png-path> [--lang a,b] [--fast]") }

let url = URL(fileURLWithPath: path)
guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
      let image = CGImageSourceCreateImageAtIndex(src, 0, nil) else {
    fail(3, "could not read image at \(path)")
}

let width = Double(image.width)
let height = Double(image.height)

let request = VNRecognizeTextRequest()
request.recognitionLevel = fast ? .fast : .accurate
// Language correction helps prose and hurts identifiers, paths and codes — the
// things worth reading off a screen. It follows the accuracy setting so `fast`
// is a single coherent choice.
request.usesLanguageCorrection = !fast
if !languages.isEmpty { request.recognitionLanguages = languages }

do {
    try VNImageRequestHandler(cgImage: image, options: [:]).perform([request])
} catch {
    fail(4, "recognition failed: \(error.localizedDescription)")
}

var lines: [[String: Any]] = []
for observation in request.results ?? [] {
    guard let best = observation.topCandidates(1).first else { continue }
    // The observation's own boundingBox, not a per-range one: under .accurate,
    // per-range boxes are a known Vision bug that returns the same rectangle
    // for every substring.
    let b = observation.boundingBox
    lines.append([
        "text": best.string,
        "confidence": Double(best.confidence),
        "box": [
            "x": b.minX * width,
            // Vision's origin is bottom-left; images are addressed top-left.
            "y": (1.0 - b.maxY) * height,
            "w": b.width * width,
            "h": b.height * height,
        ],
    ])
}

let payload: [String: Any] = ["width": image.width, "height": image.height, "lines": lines]
guard let data = try? JSONSerialization.data(withJSONObject: payload) else {
    fail(5, "could not serialise result")
}
FileHandle.standardOutput.write(data)
