# Community launch copy

Repository: https://github.com/anvsk/aimedia

## 中文短帖

我开源了 `aimedia`：一个用 Rust 编写的 AI 原生双机位直播导播引擎。

它不打算一开始复制 FFmpeg 的全部能力，而是先解决一个明确场景：两路同步的 SRT 机位经过确定性 AI 导播、音频跟随和重新编码，输出一路连续节目流。

目前发布的是开发预览：配置协议、时间戳/同步原语、自动与人工切镜状态机、VLM 建议约束、音频交叉淡化、MPEG-TS/H.264 Annex-B/AAC ADTS parser，以及 replay/bench/fuzz 工具已经可以运行。`libsrt`、NVDEC/NVENC 和 codec 数据面仍在开发，项目不会把未完成能力标成 supported。

欢迎直播基础设施、Rust、多媒体和 AI 工程方向的开发者参与，尤其需要 MPEG-TS/PES、SRT、NVIDIA Video Codec、测试语料和互操作验证方面的贡献。

https://github.com/anvsk/aimedia

## English short post

I have open-sourced `aimedia`, a Rust-based AI-native director engine for dual-camera live production.

Instead of attempting full FFmpeg compatibility on day one, it targets one concrete pipeline: two synchronized SRT camera feeds, deterministic AI-assisted shot selection, audio-follow-video, and one continuous encoded SRT program output.

This is an honest development preview. The configuration protocol, bounded sync primitives, automatic/manual director state machine, constrained VLM advisor, audio crossfade, MPEG-TS/H.264 Annex-B/AAC ADTS parsers, replay harness, benchmarks, and fuzz targets are runnable. The `libsrt`, NVDEC/NVENC, and codec data plane is still being built and is not advertised as supported.

Contributions from live-video, Rust, media-systems, and applied-AI developers are very welcome—especially around MPEG-TS/PES, SRT, NVIDIA Video Codec integration, test corpora, and interoperability testing.

https://github.com/anvsk/aimedia

## Hacker News title

Show HN: aimedia – a Rust AI-native director core for dual-camera SRT production

## Suggested tags

`rust` `video` `streaming` `srt` `mpeg-ts` `nvidia` `ai` `multimedia` `live-streaming` `ffmpeg-alternative`
