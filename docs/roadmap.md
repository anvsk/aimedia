# 市场驱动路线图

`aimedia` 面向直播开发者和集成商，负责接收摄像机或远程贡献流，完成导播、音频
处理和编码，再把连续节目发布到直播平台或直播云。项目不建设观众侧 CDN、播放器
或直播源站。

路线图按可完成的用户工作流组织，而不是按 crate 数量组织。每个里程碑只有在以下
条件同时满足后才能升级支持状态：

1. 完成至少一个已记录的用户故事；
2. 与至少两个独立外部实现互操作；
3. 达到该阶段的延迟、稳定性和资源上限；
4. 兼容证据写入支持矩阵；未验证组合保持 `experimental`。

## 当前基线：v0.1 Foundation

已完成：

- 严格的 `aimedia/v1alpha1` 配置、密钥引用和版本化本机控制协议；
- 流式 MPEG-TS 同步恢复、PSI/PES、PTS 回绕、PAT/PMT/PCR mux；
- libsrt 1.5.5 caller/listener 回环、独立节目时钟和有界队列原语；
- NVIDIA Video Codec SDK 13.0、CUDA/NVDEC/NVENC 和 libxaac 可用性探测；
- 确定性导播、音频 DSP、VLM contract、replay、bench、fuzz 和 CPU CI。

当前尚不能完成真实直播闭环。NVDEC/NVENC 帧提交、libxaac 帧处理以及持续的
SRT 数据面仍未接通。

## v0.2 Native Single-SRT

目标工作流：

```text
1x SRT/MPEG-TS -> NVDEC + AAC decode -> independent program clock
               -> NVENC + AAC encode -> MPEG-TS/SRT
```

主要交付：

- 允许一到两路输入；先启用单路真实 `aimedia run`；
- Video Codec SDK 13.0 named build context、头文件指纹和帧级 NVDEC/NVENC；
- libxaac AAC-LC 48kHz 双声道帧级解码和编码；
- 输入/输出 SRT 重连、黑帧/最后一帧与静音保活；
- OBS/FFmpeg 输入和 VLC/OBS/ffprobe 输出互操作。

完成门槛：

- 1080p30 连续运行两小时；
- 引擎新增延迟 p95 不超过 180ms，不包含 SRT latency；
- PTS、DTS、PCR 单调，输入断线不要求输出播放器重连；
- RSS 和 GPU 内存不持续增长，运行镜像没有 `ffmpeg` 或 `libav*`。

## v0.3 Dual Manual Director

- 两路持续解码、偏移、漂移校正和有界 timeline；
- 人工 Take/Auto、视频硬切、输出 IDR、音频跟随和 80ms 淡化；
- 当前路断流自动切备用，超出允许 skew 的机位不可切入；
- 健康 Take p95 小于 100ms，切镜不触发播放器重连。

达到门槛后发布 Developer Preview。

## v0.4 Cross-region Platform Baseline

- RTMP/RTMPS publish/ingest 和 FLV tag demux/mux；
- 720p/1080p、25/30/50/60fps、横竖屏和 H.264 Main/High；
- 44.1/48kHz AAC 输入归一化；
- `srt-low-latency`、`cn-rtmp-standard`、`youtube-rtmps` 和
  `twitch-rtmp` 预设；
- 腾讯云、阿里云、YouTube 非公开直播和 Twitch bandwidth test 互操作。

没有真实账号验证的抖音、快手和 Bilibili 组合保持 `experimental`。

## v0.5 Fast-brain AI Alpha

- Silero VAD、人物/人脸、嘴部运动、清晰度、曝光、冻结和传输健康度；
- 将实时指标转换为 `CameraSnapshot`，复用确定性状态机；
- 在 VLM 完全关闭时独立达到自动切镜误切和响应时间指标。

## v0.6 China Profile

- 独立 `aimedia-rtsp` 边界，支持 RTSP/RTP TCP/UDP；
- H.264/H.265 视频及 AAC/G.711 音频输入；
- 竖屏模板、腾讯云/阿里云示例和摄像机兼容矩阵；
- H.265 优先作为输入，节目发布仍以 H.264/AAC RTMP/RTMPS 为基线。

GB28181 和厂商专有 ARTC/RTS 保持插件或后续候选，不进入核心。

## v0.7 Global Low-latency Profile

- 标准 WHIP 输出、H.264/Opus、ICE/DTLS/SRTP 和 STUN/TURN；
- Bearer token 使用 secret reference，不进入 URI、配置明文或日志；
- 与 OBS 及至少两个独立 WHIP 服务互操作。

HLS ingest、HEVC/AV1 和 Enhanced RTMP 只在出现真实采用证据后排期。

## v0.8 VLM SDK 与 Production Beta

- OpenAI 兼容 VLM advisor、本地模型示例、deadline、熔断和隐私控制；
- `FastAnalyzer`、`DirectorAdvisor` 版本化 C ABI；
- 24 小时 soak、Prometheus、SBOM、升级策略和生产部署文档。

## 明确不做

- 完整 FFmpeg CLI 或全格式兼容；
- 观众侧 HTTP-FLV、HLS/LL-HLS CDN 和播放器；
- 短期内支持 GB28181、RIST、NDI、SDI、AVS3、VP9、字幕或 DRM；
- 隐式 CPU codec fallback；
- 在有真实双机位闭环前开发桌面 GUI。
