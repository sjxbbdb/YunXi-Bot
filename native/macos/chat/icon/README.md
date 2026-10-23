# 云熙 App Icon

最终方案：**熙 · 印章**。深梅色底，米纸印面，朱砂双层内框与宋体“熙”字；在小尺寸下仍保持高对比识别。

- `AppIcon-1024.png`：主图（1024×1024，透明背景）
- `AppIcon.icns`：App 包图标
- `render-icon.swift`：可复现渲染脚本（输出三版：云月 / 印章 / 对话云）

重新生成：

```bash
swift native/macos/chat/icon/render-icon.swift
# 输出 /tmp/yunxi-icon-{0,1,2}.png；印章版为 /tmp/yunxi-icon-1.png
```

设计约束：macOS 圆角方形、无界面元素复用、无外阴影、单色记号在 16/32/128px 可辨；品牌色沿用云熙玫瑰梅子。
