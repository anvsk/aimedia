# Community launch copy

Repository: https://github.com/anvsk/aimedia

> 本页是下一次发布的草稿。发帖前必须按支持矩阵重新核对完成状态，不把 `pending` 节点
> 描述为已经可用。

## 中文短帖

我在开发 `aimedia`：一个用 Rust 编写、面向长期直播作业的开源媒体运行时。

它不打算复制 FFmpeg 的全部格式，而是改变实时媒体服务的使用方式：声明输入、目标
输出、质量、延迟和故障策略，由图编译器生成可解释的有界执行计划。运行时负责节目
时钟、背压、重连、资源状态，并通过非阻塞 Tap 接入字幕、审核、识别或导播等 AI。

当前是开发预览。类型化图编译器、流式 MPEG-TS、SRT/libxaac adapter、独立节目时钟、
有界 fake runtime、导播策略、replay/bench/fuzz 已可运行；NVDEC/NVENC 帧处理和真实
单路 SRT 数据面仍在开发，不会标记为 supported。

欢迎 Rust、实时音视频、GPU 和 AI 工程方向的开发者参与，尤其需要 NVIDIA Video
Codec、SRT/RTMP/RTSP 互操作、故障注入和长时间稳定性验证方面的贡献。

https://github.com/anvsk/aimedia

## English short post

I am building `aimedia`, an open-source, service-native live media runtime in Rust.

Instead of cloning FFmpeg's full format surface, it changes how long-running media jobs are built:
declare inputs, output intent, latency, and failure policy; a compiler produces an inspectable,
bounded execution plan. The runtime owns program clocks, backpressure, reconnects, structured
state, and non-blocking AI taps.

This is an honest development preview. The typed graph compiler, streaming MPEG-TS, SRT/libxaac
adapters, independent program clocks, bounded fake runtime, optional director policy,
replay/bench, and fuzz targets are runnable. Frame-level NVDEC/NVENC and the real single-SRT data
plane are still pending and are not advertised as supported.

Contributions around Rust media systems, NVIDIA Video Codec, SRT/RTMP/RTSP interoperability,
failure injection, and soak testing are welcome.

https://github.com/anvsk/aimedia

## Hacker News title

Show HN: aimedia – an intent-compiled live media runtime in Rust

## Suggested tags

`rust` `video` `streaming` `srt` `mpeg-ts` `nvidia` `ai` `media-runtime`
