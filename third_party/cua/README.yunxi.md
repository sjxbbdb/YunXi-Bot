# Cua Driver 上游源码快照

本目录随云熙仓库保存实际接入的电脑操控底座源码，供查看、追溯和独立研究。Driver来自Cua AI，不是云熙自行实现的系统驱动。云熙适配器位于[`crates/yunxi-bot-core/src/tool/computer/`](../../crates/yunxi-bot-core/src/tool/computer/)。

| 项目 | 来源 |
| --- | --- |
| 上游仓库 | [trycua/cua](https://github.com/trycua/cua) |
| 固定发行版 | [cua-driver-rs-v0.36.0](https://github.com/trycua/cua/releases/tag/cua-driver-rs-v0.36.0) |
| 固定提交 | [`6d6c734c127025fb39ad5d585a10382808b39dc1`](https://github.com/trycua/cua/commit/6d6c734c127025fb39ad5d585a10382808b39dc1) |
| 原始源码路径 | [`libs/cua-driver`](libs/cua-driver/)（保留上游目录结构，全部文件原样复制） |
| 版权 | Copyright (c) 2025 Cua AI, Inc. |
| Driver主体许可 | [MIT](LICENSE.md) |
| 机器可读来源 | [UPSTREAM.json](UPSTREAM.json)（含上游子树Git对象哈希） |

## 保留范围与许可

保留完整Driver子项目，包括Rust原生实现、Python/TypeScript SDK、文档、测试、脚本及原有第三方声明；另保留上游根目录的[LICENSE.md](LICENSE.md)与[LICENSING.md](LICENSING.md)。本地新增文件只有本说明和`UPSTREAM.json`，上游源码无本地修改。

这不是整个Cua单体仓库的镜像：没有纳入Spaces应用、虚拟机项目、仓库级CI或其他兄弟项目。部分上游构建/发布脚本会引用这些未纳入路径；需要运行它们时应使用上述固定提交的完整上游仓库。云熙不自动执行快照中的脚本。

各项嵌入素材与可选组件遵循原始声明，见[Driver第三方声明](libs/cua-driver/rust/THIRD_PARTY_NOTICES.md)、[感知组件说明](libs/cua-driver/docs/perception-third-party-notices.md)和[模型来源与许可](libs/cua-driver/rust/crates/cua-perception/models/THIRD_PARTY_NOTICES.md)。例如可选OmniParser检测器采用AGPL-3.0-only，不能将其权重或整个Cua仓库概括为MIT。快照包含这些组件的源码和许可说明，**不包含模型权重，也没有安装或启用该扩展**。

## 与云熙运行的关系

运行时仍使用另行安装、经版本和签名校验的`/Applications/CuaDriver.app` 0.36.0。快照是独立工作区，不是云熙新增的Cargo依赖，不加入云熙编译或测试，也不会放开动作目录或审批。上游提供的功能范围大于云熙当前暴露的35项操作。

本次导入已按Git blob哈希和文件模式逐项核对，`UPSTREAM.json`保存上游`libs/cua-driver`子树哈希，可与本仓库对应子树核对。未独立构建或运行上游全部测试；云熙的测试结果不能替代Driver全平台验收。

未包含应用安装包、编译产物、模型权重、账号凭证、浏览器资料、录像或用户运行数据。
