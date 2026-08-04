# 市场驱动路线图

`aimedia` 的目标不是覆盖 FFmpeg 的所有格式，而是替代开发者在实时媒体服务中常见的
FFmpeg 子进程、Shell 拼接和外部守护逻辑：协议接入、格式归一化、转码、多路发布、
故障恢复、监控和 AI 分析接入。

每个版本必须完成至少一个真实用户故事、两个外部实现互操作和一组量化门槛。没有
真实账号、测试流或兼容证据的能力只能标记为 `experimental`。

## v0.1 Foundation：已完成基础

- 严格配置、密钥引用和版本化本机控制协议；
- 流式 MPEG-TS demux/mux、H.264 Annex-B 和 AAC ADTS；
- libsrt adapter、重连状态、独立节目时钟和有界队列；
- libxaac 帧级 adapter、NVIDIA SDK 探测和 GPU surface 所有权契约；
- fake backend 单路调度、导播策略、音频 DSP、replay、bench 和 fuzz。

这些是开发基础，不代表真实 GPU 数据面已经闭环。

## v0.2 Native Live Pipe：当前目标

完成首个可替代工作流：

```text
1x SRT/MPEG-TS -> NVDEC + AAC decode -> program timeline
               -> NVENC + AAC encode -> MPEG-TS/SRT
```

同时完成：

- 简洁 workspace 目录和 `aimedia-graph` 图编译器；
- `aimedia explain` 输出类型、内存、时钟、队列和资源计划；
- 生产 SRT/codec/scheduler 接线；
- 输入断线最后一帧加静音保活，恢复后等待 IDR；
- 输出重连后重新发送 PAT/PMT 并请求 IDR。

完成门槛：1080p30 两小时、引擎新增延迟 p95 不超过 180ms、时间戳单调、内存不持续
增长、OBS/FFmpeg 输入与 VLC/OBS/ffprobe 输出互操作，运行镜像没有 FFmpeg/libav。

## v0.3 Live Normalize and Bridge

目标用户故事：直播后端开发者把不同现场输入稳定归一化并发布到国内外平台。

- RTSP、SRT 和 RTMP 输入，SRT 和 RTMPS 输出；
- H.265 输入转 H.264 输出；
- 720p/1080p、25/30/50/60fps、横竖屏；
- 44.1/48kHz、单/双声道归一化；
- 腾讯云、阿里云、YouTube 非公开直播和 Twitch bandwidth test 互操作；
- 输入、codec 和输出故障分阶段错误及结构化指标。

这一阶段不开发桌面 GUI，也不加入任意滤镜语言。

## v0.4 Fan-out and AI Tap

- 一次解码供多个输出复用，支持每个输出独立失败和重连；
- 非阻塞视频抽样、PCM、传输指标和时间事件接口；
- analyzer deadline、采样率、权限和隐私配置；
- mock analyzer 与字幕、审核、精彩片段三个参考扩展；
- AI 变慢十倍或完全离线时，节目帧率和主链延迟不显著变化。

双机位自动导播迁移为这一接口上的官方示例，不再是版本发布前提。

## v0.5 Media Job Service

- `POST /v1/jobs`、状态、更新、停止和事件 API；
- 多作业 GPU session、显存和带宽准入；
- 输出配置热更新，尽量不重启输入和共享解码；
- Prometheus/OpenTelemetry、优雅 drain、24 小时 soak；
- 容器镜像、SBOM、升级策略和生产部署文档。

## v0.6 Regional Profiles

中国大陆优先验证 RTSP 摄像机、H.265 输入、腾讯云和阿里云 RTMP/RTMPS。海外优先
验证 SRT/RTMP 输入、YouTube/Twitch RTMPS；WHIP 在至少两个真实服务可测试后进入。

GB28181、厂商专有 ARTC/RTS、RIST、NDI、SDI、HLS CDN、AVS3、DRM 和播放器不进入
当前核心路线，只有真实采用数据足够时才以插件或独立里程碑评估。

## v0.7 Extension SDK and Beta

- transport、codec 与 GPU 的版本化 native ABI；
- analyzer、policy 和事件扩展 SDK；
- VLM adapter、deadline、熔断和隐私控制；
- 24 小时 soak、故障注入、兼容矩阵和稳定配置升级路径。

## 长期判断标准

新增功能必须至少满足一项：完成高频市场工作流、减少一次昂贵转码或内存复制、降低
运维故障率、让 AI 安全接入实时主链。仅仅因为 FFmpeg 支持某格式，不构成 aimedia
实现它的理由。
