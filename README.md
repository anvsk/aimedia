# aimedia

[English](README.en.md) | 简体中文

[![CI](https://github.com/anvsk/aimedia/actions/workflows/ci.yml/badge.svg)](https://github.com/anvsk/aimedia/actions/workflows/ci.yml)
[![Fuzz](https://github.com/anvsk/aimedia/actions/workflows/fuzz.yml/badge.svg)](https://github.com/anvsk/aimedia/actions/workflows/fuzz.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

`aimedia` 是一个面向开发者和集成商的开源实时媒体运行时：声明输入、输出、质量、
延迟和故障策略，系统负责生成执行计划、运行有界媒体图、监控资源并恢复长期直播。

> 它不重写所有 codec，也不追求 FFmpeg 的全格式覆盖。首要目标是替代实时媒体服务中
> 常见的 FFmpeg 子进程、Shell 拼接和外部守护逻辑，并为 AI 分析提供不会阻塞直播的
> 标准接口。

## 核心差异

- **目标优先**：用户描述想要的结果，图编译器生成具体节点和连接。
- **启动前可解释**：`aimedia explain` 展示媒体类型、内存位置、时钟域、队列上限和
  GPU session 需求。
- **长期运行优先**：重连、背压、故障域、结构化状态和受控关闭属于运行时本身。
- **时间是一等公民**：输入时间戳只用于映射，输出由独立单调节目时钟产生。
- **AI 不进关键链路**：字幕、审核、识别和导播通过非阻塞 Analyzer Tap 接入。
- **支持范围诚实**：没有真实互操作和稳定性证据的组合不会标记为 supported。

## 当前状态

当前版本是 `0.1.0-alpha.1` foundation，已经实现：

- `aimedia-graph`：把现有配置编译为类型化、有界的执行计划；
- 流式 MPEG-TS demux/mux、PSI/PES、PTS 回绕、PAT/PMT/PCR；
- libsrt caller/listener adapter、重连状态和敏感信息校验；
- libxaac AAC-LC 帧级 adapter、1024-sample 时间线和 native round-trip；
- 独立节目时钟、固定容量单路调度器、fake transport/codec 验证；
- NVIDIA SDK 探测、GPU surface lease 契约、本机控制协议；
- 双输入确定性导播、音频 DSP、VLM contract、replay、bench 和 fuzz。

尚未完成，也不会伪装为已经完成：

- NVDEC/NVENC 帧提交和 GPU surface copy；
- 生产 SRT、codec 与调度器的完整接线；
- 断流期间最后一帧和静音保活；
- 真实单路 GPU 持续输出、互操作和两小时 soak；
- 通用 `MediaJob` v2 配置、多输出和 Analyzer Tap 数据面。

因此当前 `aimedia run` 默认仍会返回明确的 pending 错误；实时能力以
[支持矩阵](docs/support-matrix.md)为准。

## 快速体验

要求 Rust 1.85 或更新版本。CPU 环境可以检查配置和执行计划：

```bash
cargo run -p aimedia -- explain -f examples/single-srt.yaml
cargo run -p aimedia -- explain -f examples/single-srt.yaml --json
cargo run -p aimedia -- run -f examples/single-srt.yaml --dry-run
cargo run -p aimedia -- run -f examples/single-srt.yaml --mock
cargo run -p aimedia -- doctor --json
```

双机位导播已经降为可选扩展示例：

```bash
cargo run -p aimedia -- replay examples/replay.jsonl -f examples/director.yaml
cargo run -p aimedia -- bench -f examples/director.yaml \
  --capture examples/replay.jsonl --iterations 1000
```

## Workspace

目录不重复项目名前缀，公开 Cargo 包名保留 `aimedia-*`：

```text
crates/
  aac/       libxaac adapter
  cli/       aimedia 命令行
  core/      媒体、时间、控制和扩展契约
  graph/     目标配置到 ExecutionPlan 的编译器
  mpegts/    clean-room MPEG-TS
  nvidia/    CUDA、NVDEC、NVENC 边界
  runtime/   有界执行器和作业控制
  srt/       libsrt adapter
```

## 设计原则

1. 配置描述目标，执行计划描述具体步骤。
2. 所有媒体队列都有固定容量和明确的满载策略。
3. 内存域、时钟域、延迟和故障策略属于图连接契约。
4. AI、VLM 和业务策略永远不能成为节目输出的存活条件。
5. codec 与 transport 通过窄 FFI 边界复用成熟实现。
6. 未通过外部互操作、故障注入和 soak 的能力不得升级为 supported。

进一步阅读：

- [RFC 0001：意图驱动的实时媒体运行时](docs/rfcs/0001-intent-media-runtime.md)
- [架构说明](docs/architecture.md)
- [市场驱动路线图](docs/roadmap.md)
- [用户故事](docs/user-stories.md)
- [中文快速入门](docs/getting-started.zh-CN.md)
- [直播媒体术语表](docs/glossary.md)
- [设计取舍](docs/design-rationale.md)
- [市场支持策略](docs/market-support.md)
- [支持矩阵](docs/support-matrix.md)

## 许可证

核心代码采用 Apache License 2.0。外部 transport、模型、codec 和 NVIDIA 运行时保留
各自许可证。开源许可证不等于 H.264/AAC 专利授权，商业发行前需要独立法律评估。
