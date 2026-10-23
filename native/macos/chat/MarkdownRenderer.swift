import AppKit

/// 轻量 Markdown 渲染：参考 Codex 前端的 markdown/codeblock 样式，
/// 只覆盖聊天里常见的行内强调、代码、列表、引用与链接，不引入渲染依赖。
enum MarkdownRenderer {
    static let bodyFont = NSFont.systemFont(ofSize: 14)
    static let boldFont = NSFont.systemFont(ofSize: 14, weight: .semibold)
    static let headingFont = NSFont.systemFont(ofSize: 15, weight: .semibold)
    static let monoFont = NSFont.monospacedSystemFont(ofSize: 12.5, weight: .regular)

    static func render(_ text: String, isUser: Bool) -> NSAttributedString {
        let textColor: NSColor = isUser ? .white : .labelColor
        let secondary: NSColor = isUser ? NSColor.white.withAlphaComponent(0.78) : .secondaryLabelColor
        let codeBackground: NSColor = isUser
            ? NSColor.white.withAlphaComponent(0.16)
            : NSColor.adaptive(light: NSColor(calibratedWhite: 0.90, alpha: 1), dark: NSColor(calibratedWhite: 0.26, alpha: 1))

        let result = NSMutableAttributedString()
        var inCodeBlock = false
        let lines = text.components(separatedBy: "\n")
        for (index, raw) in lines.enumerated() {
            let paragraph = NSMutableParagraphStyle()
            paragraph.lineSpacing = 3
            paragraph.paragraphSpacing = index == lines.count - 1 ? 0 : 2

            let trimmed = raw.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("```") {
                inCodeBlock.toggle()
                continue
            }

            if inCodeBlock {
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: monoFont,
                    .foregroundColor: textColor,
                    .backgroundColor: codeBackground,
                    .paragraphStyle: paragraph,
                ]
                result.append(NSAttributedString(string: raw + "\n", attributes: attrs))
                continue
            }

            if trimmed.isEmpty {
                result.append(NSAttributedString(string: "\n", attributes: [.font: bodyFont]))
                continue
            }

            if trimmed.hasPrefix("### ") || trimmed.hasPrefix("## ") || trimmed.hasPrefix("# ") {
                let heading = trimmed.replacingOccurrences(of: "^#{1,3} ", with: "", options: .regularExpression)
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: headingFont,
                    .foregroundColor: textColor,
                    .paragraphStyle: paragraph,
                ]
                result.append(NSAttributedString(string: heading + "\n", attributes: attrs))
                continue
            }

            if trimmed.hasPrefix("> ") {
                let content = String(trimmed.dropFirst(2))
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: bodyFont,
                    .foregroundColor: secondary,
                    .paragraphStyle: paragraph,
                ]
                result.append(NSAttributedString(string: "▎ ", attributes: attrs))
                result.append(inline(content, textColor: textColor, secondary: secondary, codeBackground: codeBackground, paragraph: paragraph))
                result.append(NSAttributedString(string: "\n", attributes: attrs))
                continue
            }

            if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") {
                let content = String(trimmed.dropFirst(2))
                result.append(NSAttributedString(string: "• ", attributes: [.font: bodyFont, .foregroundColor: secondary, .paragraphStyle: paragraph]))
                result.append(inline(content, textColor: textColor, secondary: secondary, codeBackground: codeBackground, paragraph: paragraph))
                result.append(NSAttributedString(string: "\n", attributes: [.font: bodyFont, .paragraphStyle: paragraph]))
                continue
            }

            result.append(inline(trimmed, textColor: textColor, secondary: secondary, codeBackground: codeBackground, paragraph: paragraph))
            result.append(NSAttributedString(string: "\n", attributes: [.font: bodyFont, .paragraphStyle: paragraph]))
        }
        return result
    }

    private static func inline(
        _ line: String,
        textColor: NSColor,
        secondary: NSColor,
        codeBackground: NSColor,
        paragraph: NSParagraphStyle
    ) -> NSMutableAttributedString {
        let base: [NSAttributedString.Key: Any] = [
            .font: bodyFont,
            .foregroundColor: textColor,
            .paragraphStyle: paragraph,
        ]
        let out = NSMutableAttributedString()
        var index = line.startIndex

        func appendPlain(_ text: String) {
            guard !text.isEmpty else { return }
            out.append(NSAttributedString(string: text, attributes: base))
        }

        while index < line.endIndex {
            let rest = line[index...]

            if rest.hasPrefix("`") {
                let after = line.index(after: index)
                if let close = line[after...].firstIndex(of: "`") {
                    let code = String(line[after..<close])
                    var attrs = base
                    attrs[.font] = monoFont
                    attrs[.backgroundColor] = codeBackground
                    out.append(NSAttributedString(string: code, attributes: attrs))
                    index = line.index(after: close)
                    continue
                }
            }

            if rest.hasPrefix("**") {
                let after = line.index(index, offsetBy: 2)
                if let close = line.range(of: "**", range: after..<line.endIndex) {
                    var attrs = base
                    attrs[.font] = boldFont
                    out.append(NSAttributedString(string: String(line[after..<close.lowerBound]), attributes: attrs))
                    index = close.upperBound
                    continue
                }
            }

            if rest.hasPrefix("[") {
                if let closeBracket = line[index...].firstIndex(of: "]"),
                   let openParen = line.index(closeBracket, offsetBy: 1, limitedBy: line.endIndex),
                   openParen < line.endIndex,
                   line[openParen] == "(",
                   let closeParen = line[openParen...].firstIndex(of: ")") {
                    let label = String(line[line.index(after: index)..<closeBracket])
                    var attrs = base
                    attrs[.foregroundColor] = NSColor.yunxiAccent
                    attrs[.underlineStyle] = NSUnderlineStyle.single.rawValue
                    out.append(NSAttributedString(string: label, attributes: attrs))
                    index = line.index(after: closeParen)
                    continue
                }
            }

            if rest.hasPrefix("*") {
                let after = line.index(after: index)
                if let close = line[after...].firstIndex(of: "*") {
                    let inner = String(line[after..<close])
                    var attrs = base
                    let italic = NSFontManager.shared.convert(bodyFont, toHaveTrait: .italicFontMask)
                    attrs[.font] = italic
                    out.append(NSAttributedString(string: inner, attributes: attrs))
                    index = line.index(after: close)
                    continue
                }
            }

            appendPlain(String(line[index]))
            index = line.index(after: index)
        }
        return out
    }
}
