# browser-use 参考与许可

来源：https://github.com/browser-use/browser-use

固定提交：`aa093a843da5edbf323c0b4145c9b6a0db3ecde8`，MIT，Copyright (c) 2024 Gregor Zunic。

云熙的 `crates/yunxi-bot-core/src/tool/progress.rs` 以 Rust 重新实现了 `browser_use/agent/views.py` 中“同一页面指纹连续不变时提示重新规划”的机制；`think/prompt.rs` 借鉴 `agent/message_manager/service.py` 仅保留当前界面状态的方式。没有引入 Python Agent、浏览器执行器、模型权重或运行依赖。

此目录保留原始 [LICENSE](LICENSE)。实际参考文件与取舍见 [补强记录](../../docs/通用电脑操控补强-20261018.md)。
