import AppKit
import Foundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("macOS icon rendering failed: \(message)\n".utf8))
    exit(EXIT_FAILURE)
}

guard CommandLine.arguments.count == 4 else {
    fail("usage: render-macos-icon <input.png> <output.png> <pixel-size>")
}

let inputURL = URL(fileURLWithPath: CommandLine.arguments[1])
let outputURL = URL(fileURLWithPath: CommandLine.arguments[2])
guard let pixelSize = Int(CommandLine.arguments[3]), pixelSize > 0, pixelSize <= 4096 else {
    fail("pixel-size must be between 1 and 4096")
}
guard let source = NSImage(contentsOf: inputURL), source.isValid else {
    fail("could not read \(inputURL.path)")
}
guard let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil,
    pixelsWide: pixelSize,
    pixelsHigh: pixelSize,
    bitsPerSample: 8,
    samplesPerPixel: 4,
    hasAlpha: true,
    isPlanar: false,
    colorSpaceName: NSColorSpaceName.deviceRGB,
    bytesPerRow: pixelSize * 4,
    bitsPerPixel: 32
) else {
    fail("could not allocate the \(pixelSize)x\(pixelSize) bitmap")
}
guard let context = NSGraphicsContext(bitmapImageRep: bitmap) else {
    fail("could not create a bitmap graphics context")
}

bitmap.size = NSSize(width: pixelSize, height: pixelSize)
let destination = NSRect(x: 0, y: 0, width: pixelSize, height: pixelSize)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = context
context.imageInterpolation = NSImageInterpolation.high
NSColor.clear.setFill()
destination.fill()
source.draw(
    in: destination,
    from: NSRect(origin: .zero, size: source.size),
    operation: .sourceOver,
    fraction: 1.0,
    respectFlipped: false,
    hints: [.interpolation: NSImageInterpolation.high]
)
context.flushGraphics()
NSGraphicsContext.restoreGraphicsState()

guard let png = bitmap.representation(using: NSBitmapImageRep.FileType.png, properties: [:]) else {
    fail("could not encode the rendered icon as PNG")
}

do {
    try png.write(to: outputURL, options: Data.WritingOptions.atomic)
} catch {
    fail("could not write \(outputURL.path): \(error)")
}
