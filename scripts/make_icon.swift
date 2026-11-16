// Regenerates assets/icon.png (1024×1024) with the "mYT" logo:
//   swift scripts/make_icon.swift [text]
// Coral rounded square (macOS icon grid: 824 px body), white SF Pro Rounded
// lettering: a lighter lowercase "m" and a heavy "YT".
import AppKit

let text = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "mYT"
let N: CGFloat = 1024, body: CGFloat = 824, off = (N - body) / 2, radius: CGFloat = 185

let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(N), pixelsHigh: Int(N), bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                           bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

let rect = NSRect(x: off, y: off, width: body, height: body)
let shape = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
NSGradient(starting: NSColor(srgbRed: 1.0, green: 0.48, blue: 0.36, alpha: 1),
           ending: NSColor(srgbRed: 0.91, green: 0.23, blue: 0.31, alpha: 1))!.draw(in: shape, angle: -90)

func rounded(_ size: CGFloat, _ weight: NSFont.Weight) -> NSFont {
    let base = NSFont.systemFont(ofSize: size, weight: weight)
    if let d = base.fontDescriptor.withDesign(.rounded), let f = NSFont(descriptor: d, size: size) { return f }
    return base
}
let first = String(text.prefix(1)), rest = String(text.dropFirst())
let s = NSMutableAttributedString()
s.append(NSAttributedString(string: first, attributes: [
    .font: rounded(330, .semibold), .foregroundColor: NSColor.white.withAlphaComponent(0.88)]))
s.append(NSAttributedString(string: rest, attributes: [
    .font: rounded(330, .heavy), .foregroundColor: NSColor.white, .kern: -6]))
let size = s.size()
// optical centre: cap-height based, a touch above the geometric middle
let font = rounded(330, .heavy)
let y = N / 2 - (font.capHeight / 2) + font.descender - 6
s.draw(at: NSPoint(x: (N - size.width) / 2, y: y + 0))

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "assets/icon.png"))
print("wrote assets/icon.png (\(text))")
