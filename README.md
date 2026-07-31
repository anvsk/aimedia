# aimedia

[English](README.en.md) | 简体中文

[![CI](https://github.com/anvsk/aimedia/actions/workflows/ci.yml/badge.svg)](https://github.com/anvsk/aimedia/actions/workflows/ci.yml)
[![Fuzz](https://github.com/anvsk/aimedia/actions/workflows/fuzz.yml/badge.svg)](https://github.com/anvsk/aimedia/actions/workflows/fuzz.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

`aimedia` 是一个面向直播开发者和集成商的开源、AI 原生制作与发布引擎。它接收
摄像机或远程贡献流，完成导播、音频处理和编码，再把连续节目发布到直播平台或
直播云。长期目标是在明确的市场工作流内逐步替代 FFmpeg，而不是追求未完成的
“全格式兼容”。

> 开发预览：流式 TS、SRT adapter、libxaac 帧处理、节目时钟和本机控制面已经进入
> 第二阶段开发；NVDEC/NVENC 帧提交和完整实时数据面尚未闭环。项目名是工作名，
> 公开品牌使用前仍需完成名称与商标检索。

## 当前状态

这是 `0.1.0-alpha.1` 基础版本。当前已经实现：

- `aimedia/v1alpha1` 严格 YAML 配置与密钥引用校验；
- 单调时间戳映射和固定容量双路同步缓冲原语；
- 带最短镜头、迟滞、候选保持、故障切换和人工 `take` 的双机位状态机；
- VLM 建议 25% 权重上限、有效期校验和 OpenAI 兼容接口；
- 48kHz BS.1770 K-weighting 滚动响度估计、增益计算和 80ms 等功率音频切换；
- clean-room MPEG-TS 流式同步、PSI/PES 重组、PTS 回绕、PAT/PMT/PCR、TS mux、H.264 Annex-B 和 AAC ADTS；
- 独立节目时钟、队列容量计算、1ms/s 漂移校正和版本化 Unix Socket 控制面；
- 运行时加载的 libsrt caller/listener/epoll/加密/统计边界、指数退避重连和 NVIDIA
  环境探测；
- libxaac AAC-LC 帧级解码/编码、1024-sample cadence、flush 和 native round-trip；
- 一到两路输入配置、单路运行状态，以及队列/codec/GPU/SRT 可观测性契约；
- `probe`、`doctor`、`control`、`run --dry-run`、`run --mock`、`explain`、`replay`、`bench` CLI。
- MPEG-TS、H.264/AAC elementary stream 和配置协议的 Linux fuzz 入口。

尚未实现、也不会伪装成已经实现：

- SRT 与 codec 串接后的持续节目输出；
- NVDEC/NVENC 帧提交和 GPU surface copy；
- codec、节目时钟、mux 和 SRT 之间的持续调度与重连；
- Silero/视觉 ONNX 分析器；
- native codec 帧级 API 和动态插件函数表。

这些边界已经在 `aimedia-core::backend` 中定义，后续实现不需要把 FFmpeg 引入运行时。

## 快速体验

要求 Rust 1.85 或更新版本。

```bash
cargo run -p aimedia -- explain -f examples/director.yaml
cargo run -p aimedia -- explain -f examples/single-srt.yaml --json
cargo run -p aimedia -- doctor --json
cargo run -p aimedia -- run -f examples/director.yaml --dry-run
cargo run -p aimedia -- run -f examples/director.yaml --mock
cargo run -p aimedia -- replay examples/replay.jsonl -f examples/director.yaml
cargo run -p aimedia -- bench -f examples/director.yaml \
  --capture examples/replay.jsonl --iterations 1000
cargo run -p aimedia -- probe sample.ts --json
docker build -t aimedia:dev .
```

当前单路原生 `run` 会返回 `nativeVideoBackendPending`，双路会返回
`dualDataPlanePending`；`--mock` 会运行真实节目调度器和 Unix Socket，但不发送媒体。
实时能力的完成情况以[支持矩阵](docs/support-matrix.md)为准。

## 设计原则

1. 媒体主链永远不等待 VLM。
2. 所有数据队列必须有固定容量和明确的丢弃/故障策略。
3. 输出使用独立单调节目时钟，禁止拼接不同输入的 PTS。
4. AI 只提供有时限的分数和事件，确定性状态机拥有最终切镜权。
5. MPEG-TS 和媒体协议基于公开规范独立实现，不复制 FFmpeg 代码。
6. 配置和日志不接受明文密码、token 或 passphrase。
7. 未通过兼容测试的能力不得标记为 supported。

模糊测试使用 Linux nightly：

```bash
cargo install cargo-fuzz --locked
cargo fuzz run mpegts
cargo fuzz run elementary
cargo fuzz run config
```

进一步阅读：

- [中文快速入门](docs/getting-started.zh-CN.md)
- [用户故事与验收场景](docs/user-stories.md)
- [中国大陆与海外市场支持策略](docs/market-support.md)
- [平台预设契约](docs/platform-presets.md)
- [直播媒体术语表](docs/glossary.md)
- [为什么采用这套架构](docs/design-rationale.md)
- [架构说明](docs/architecture.md)、[路线图](docs/roadmap.md)和[支持矩阵](docs/support-matrix.md)

## 许可证

核心代码采用 Apache License 2.0。外部传输、模型、codec 和 NVIDIA 运行时保留各自许可证。开源许可证不等于 H.264/AAC 专利授权，商业发行前需要独立法律评估。
