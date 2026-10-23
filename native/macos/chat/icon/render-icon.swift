import AppKit

let S: CGFloat = 1024

func squircle(_ rect: NSRect, n: CGFloat = 5) -> NSBezierPath {
    let p = NSBezierPath()
    let cx = rect.midX, cy = rect.midY
    let a = rect.width / 2, b = rect.height / 2
    let steps = 720
    for i in 0...steps {
        let t = CGFloat(i) / CGFloat(steps) * .pi * 2
        let ct = cos(t), st = sin(t)
        let x = cx + a * copysign(pow(abs(ct), 2 / n), ct)
        let y = cy + b * copysign(pow(abs(st), 2 / n), st)
        if i == 0 { p.move(to: NSPoint(x: x, y: y)) } else { p.line(to: NSPoint(x: x, y: y)) }
    }
    p.close()
    return p
}

func gradient(_ colors: [NSColor], angle: CGFloat) -> NSGradient { NSGradient(colors: colors)! }

func sparkle(center: NSPoint, radius: CGFloat, color: NSColor) {
    let p = NSBezierPath()
    p.move(to: NSPoint(x: center.x, y: center.y + radius))
    p.curve(to: NSPoint(x: center.x + radius, y: center.y),
            controlPoint1: NSPoint(x: center.x + radius * 0.18, y: center.y + radius * 0.18),
            controlPoint2: NSPoint(x: center.x + radius * 0.18, y: center.y + radius * 0.18))
    p.curve(to: NSPoint(x: center.x, y: center.y - radius),
            controlPoint1: NSPoint(x: center.x + radius * 0.18, y: center.y - radius * 0.18),
            controlPoint2: NSPoint(x: center.x + radius * 0.18, y: center.y - radius * 0.18))
    p.curve(to: NSPoint(x: center.x - radius, y: center.y),
            controlPoint1: NSPoint(x: center.x - radius * 0.18, y: center.y - radius * 0.18),
            controlPoint2: NSPoint(x: center.x - radius * 0.18, y: center.y - radius * 0.18))
    p.curve(to: NSPoint(x: center.x, y: center.y + radius),
            controlPoint1: NSPoint(x: center.x - radius * 0.18, y: center.y + radius * 0.18),
            controlPoint2: NSPoint(x: center.x - radius * 0.18, y: center.y + radius * 0.18))
    p.close()
    color.setFill(); p.fill()
}

func cloudPath() -> NSBezierPath {
    let p = NSBezierPath()
    p.appendOval(in: NSRect(x: 238, y: 352, width: 238, height: 238))
    p.appendOval(in: NSRect(x: 382, y: 300, width: 318, height: 318))
    p.appendOval(in: NSRect(x: 604, y: 362, width: 198, height: 198))
    p.appendRoundedRect(NSRect(x: 232, y: 330, width: 578, height: 262), xRadius: 126, yRadius: 126)
    // 对话尾巴
    p.move(to: NSPoint(x: 330, y: 372))
    p.curve(to: NSPoint(x: 252, y: 226), controlPoint1: NSPoint(x: 305, y: 300), controlPoint2: NSPoint(x: 276, y: 252))
    p.curve(to: NSPoint(x: 430, y: 340), controlPoint1: NSPoint(x: 335, y: 278), controlPoint2: NSPoint(x: 380, y: 316))
    p.close()
    return p
}

func drawTile(_ variant: Int) -> NSImage {
    let image = NSImage(size: NSSize(width: S, height: S))
    image.lockFocus()
    defer { image.unlockFocus() }
    guard let ctx = NSGraphicsContext.current else { return image }
    ctx.imageInterpolation = .high

    let tile = NSRect(x: 72, y: 72, width: 880, height: 880)
    let shell = squircle(tile)

    NSGraphicsContext.saveGraphicsState()
    shell.addClip()

    if variant == 2 {
        gradient([NSColor(calibratedRed: 0.30, green: 0.08, blue: 0.12, alpha: 1),
                  NSColor(calibratedRed: 0.55, green: 0.18, blue: 0.24, alpha: 1),
                  NSColor(calibratedRed: 0.42, green: 0.10, blue: 0.16, alpha: 1)], angle: -70).draw(in: tile, angle: -70)
    } else {
        gradient([NSColor(calibratedRed: 0.33, green: 0.10, blue: 0.15, alpha: 1),
                  NSColor(calibratedRed: 0.24, green: 0.07, blue: 0.11, alpha: 1),
                  NSColor(calibratedRed: 0.14, green: 0.04, blue: 0.07, alpha: 1)], angle: -75).draw(in: tile, angle: -75)
        // 背景微光
        let glow = NSGradient(colors: [NSColor(calibratedRed: 0.78, green: 0.36, blue: 0.46, alpha: 0.34),
                                       NSColor(calibratedRed: 0.78, green: 0.36, blue: 0.46, alpha: 0.0)])!
        glow.draw(in: NSBezierPath(ovalIn: NSRect(x: 300, y: 300, width: 620, height: 620)), relativeCenterPosition: .zero)
    }

    if variant == 0 {
        // A: 月 + 云
        let moon = NSBezierPath(ovalIn: NSRect(x: 570, y: 500, width: 210, height: 210))
        NSColor(calibratedRed: 0.96, green: 0.79, blue: 0.60, alpha: 1).setFill()
        moon.fill()
        NSColor(calibratedRed: 1.0, green: 0.88, blue: 0.70, alpha: 0.35).setFill()
        NSBezierPath(ovalIn: NSRect(x: 535, y: 465, width: 280, height: 280)).fill()

        let cloud = cloudPath()
        let shadow = NSShadow(); shadow.shadowColor = NSColor.black.withAlphaComponent(0.30)
        shadow.shadowBlurRadius = 34; shadow.shadowOffset = NSSize(width: 0, height: -18)
        shadow.set()
        NSColor(calibratedRed: 0.99, green: 0.95, blue: 0.90, alpha: 1).setFill()
        cloud.fill()
        NSGraphicsContext.restoreGraphicsState()
        NSGraphicsContext.saveGraphicsState()
        shell.addClip()
        // 云内部柔光
        let cloudGrad = NSGradient(colors: [NSColor(calibratedRed: 1.0, green: 0.99, blue: 0.97, alpha: 1),
                                            NSColor(calibratedRed: 0.94, green: 0.83, blue: 0.77, alpha: 1)])!
        cloudGrad.draw(in: cloud, angle: -90)
        // 脸颊
        NSColor(calibratedRed: 0.85, green: 0.47, blue: 0.53, alpha: 0.18).setFill()
        NSBezierPath(ovalIn: NSRect(x: 430, y: 420, width: 100, height: 50)).fill()
        NSBezierPath(ovalIn: NSRect(x: 600, y: 420, width: 100, height: 50)).fill()
    } else if variant == 1 {
        // B: 熙字印章（最终版：纸感、柔和投影、内描边）
        let slabRect = NSRect(x: 172, y: 172, width: 680, height: 680)
        let slab = NSBezierPath(roundedRect: slabRect, xRadius: 156, yRadius: 156)
        let slabShadow = NSShadow()
        slabShadow.shadowColor = NSColor.black.withAlphaComponent(0.34)
        slabShadow.shadowBlurRadius = 34
        slabShadow.shadowOffset = NSSize(width: 0, height: -16)
        slabShadow.set()
        let paper = NSGradient(colors: [NSColor(calibratedRed: 0.995, green: 0.972, blue: 0.930, alpha: 1),
                                        NSColor(calibratedRed: 0.955, green: 0.895, blue: 0.835, alpha: 1)])!
        paper.draw(in: slab, angle: -90)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.restoreGraphicsState()
        // 纸边
        NSColor(calibratedRed: 0.88, green: 0.75, blue: 0.64, alpha: 0.85).setStroke()
        slab.lineWidth = 2; slab.stroke()
        // 朱砂内框
        let inner = NSBezierPath(roundedRect: NSRect(x: 212, y: 212, width: 600, height: 600), xRadius: 120, yRadius: 120)
        NSColor(calibratedRed: 0.62, green: 0.14, blue: 0.19, alpha: 0.72).setStroke()
        inner.lineWidth = 4; inner.stroke()
        let inner2 = NSBezierPath(roundedRect: NSRect(x: 230, y: 230, width: 564, height: 564), xRadius: 104, yRadius: 104)
        NSColor(calibratedRed: 0.62, green: 0.14, blue: 0.19, alpha: 0.28).setStroke()
        inner2.lineWidth = 1.5; inner2.stroke()

        let text = "熙" as NSString
        let font = NSFont(name: "Songti SC", size: 430) ?? NSFont.systemFont(ofSize: 430, weight: .bold)
        let plum = NSColor(calibratedRed: 0.55, green: 0.10, blue: 0.15, alpha: 1)
        let shadowAttrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor.white.withAlphaComponent(0.55),
            .shadow: { let s = NSShadow(); s.shadowColor = NSColor.white.withAlphaComponent(0.55); s.shadowBlurRadius = 0; s.shadowOffset = NSSize(width: 0, height: -3); return s }(),
        ]
        let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: plum]
        let size = text.size(withAttributes: attrs)
        let origin = NSPoint(x: 512 - size.width / 2, y: 512 - size.height / 2 + 16)
        text.draw(at: NSPoint(x: origin.x, y: origin.y + 2), withAttributes: shadowAttrs)
        text.draw(at: origin, withAttributes: attrs)
    } else {
        // C: 对话云 + 星
        let moon = NSBezierPath(ovalIn: NSRect(x: 588, y: 470, width: 220, height: 220))
        NSColor(calibratedRed: 0.88, green: 0.42, blue: 0.48, alpha: 0.22).setFill(); moon.fill()
        let cloud = cloudPath()
        let shadow = NSShadow(); shadow.shadowColor = NSColor.black.withAlphaComponent(0.32)
        shadow.shadowBlurRadius = 36; shadow.shadowOffset = NSSize(width: 0, height: -20)
        shadow.set()
        let cloudGrad = NSGradient(colors: [NSColor(calibratedRed: 1.0, green: 0.99, blue: 0.96, alpha: 1),
                                            NSColor(calibratedRed: 0.95, green: 0.84, blue: 0.79, alpha: 1)])!
        cloudGrad.draw(in: cloud, angle: -88)
        // 云上的小星
        sparkle(center: NSPoint(x: 722, y: 612), radius: 62, color: NSColor(calibratedRed: 0.99, green: 0.83, blue: 0.52, alpha: 1))
        sparkle(center: NSPoint(x: 758, y: 662), radius: 24, color: NSColor(calibratedRed: 1.0, green: 0.92, blue: 0.72, alpha: 0.95))
        // 云侧高光
        NSColor.white.withAlphaComponent(0.22).setFill()
        NSBezierPath(ovalIn: NSRect(x: 320, y: 500, width: 160, height: 110)).fill()
    }

    NSGraphicsContext.restoreGraphicsState()

    // 顶部高光与内描边
    let highlight = NSGradient(colors: [NSColor.white.withAlphaComponent(0.13), NSColor.white.withAlphaComponent(0)])!
    highlight.draw(in: NSBezierPath(rect: tile), angle: -90)
    NSColor.white.withAlphaComponent(0.16).setStroke()
    shell.lineWidth = 3
    shell.stroke()

    return image
}

func savePNG(_ image: NSImage, _ path: String) {
    guard let tiff = image.tiffRepresentation,
          let rep = NSBitmapImageRep(data: tiff),
          let data = rep.representation(using: .png, properties: [:]) else { return }
    try? data.write(to: URL(fileURLWithPath: path))
}

for i in 0..<3 {
    let image = drawTile(i)
    savePNG(image, "/tmp/yunxi-icon-\(i).png")
}

// contact sheet
let sheet = NSImage(size: NSSize(width: 1536, height: 640))
sheet.lockFocus()
NSColor(calibratedWhite: 0.10, alpha: 1).setFill(); NSRect(x: 0, y: 0, width: 1536, height: 640).fill()
for i in 0..<3 {
    let img = NSImage(contentsOfFile: "/tmp/yunxi-icon-\(i).png")!
    img.draw(in: NSRect(x: 32 + i * 512, y: 48, width: 448, height: 448))
    let label = ["A 云与月", "B 熙 · 印章", "C 对话云 · 星"][i] as NSString
    let attrs: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: 22, weight: .medium), .foregroundColor: NSColor(calibratedWhite: 0.82, alpha: 1)]
    let sz = label.size(withAttributes: attrs)
    let labelX: CGFloat = CGFloat(32 + i * 512) + (448 - sz.width) / 2
    label.draw(at: NSPoint(x: labelX, y: 14), withAttributes: attrs)
}
sheet.unlockFocus()
savePNG(sheet, "/tmp/yunxi-icons-contact.png")
print("rendered")
