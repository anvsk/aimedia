# HEVC 输入桥接验证记录

更新时间：2026-08-11

## 结论

V3-04A—D 已完成。软件 RTSP HEVC Main 输入已经在 RTX 5060 Laptop + 577.12 驱动上，
经过真实 NVDEC 解码、共享 NV12 surface、NVENC H.264 编码和 MPEG-TS/SRT 输出。短门禁
同时覆盖发布源断开/恢复，以及 40ms RTT、20ms 抖动和 1% 丢包。

该子能力从 `foundation` 升为 `experimental`，不是 `supported`：当前证据来自
MediaMTX 1.20.0 与 libx265 软件源，尚未完成物理摄像机认证和两小时长稳。SRT/TS 和
传统 RTMP 输入仍只接受 H.264；本阶段没有增加 HEVC 输出或 Enhanced RTMP。

## 通过的 90 秒门禁

运行命令：

```powershell
pwsh ./tools/rtsp.ps1 `
  -EngineImage aimedia:hevc-v3-04c-fix2 `
  -PeerImage aimedia:test-tools `
  -VideoCodec hevc `
  -DurationSeconds 90 `
  -FaultAtSeconds 20 -FaultSeconds 5 `
  -ImpairAtSeconds 50 -ImpairSeconds 10 `
  -SampleIntervalSeconds 5
```

环境与产物：

- GPU：NVIDIA GeForce RTX 5060 Laptop；驱动 577.12。
- Video Codec SDK：13.0；固定头文件组合 SHA-256
  `613e2cd436d4d7fbc283e5d92184e7d7f8739ec680f1ee372d580eb801df9ef2`。
- 引擎镜像：
  `sha256:b052acc128a41b43dd68cfe02d6843cf68f27b0829c5e26225907fec648d1ac7`。
- 汇总：`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtsp-062ea84c\summary.json`；
  SHA-256 `818C55B4DA6AD8916C506EF55FF4FCF9EEBB129F412336C49D9FA4D6FEAA4283`。
- 样本：`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtsp-062ea84c\samples.jsonl`；
  SHA-256 `7C71C82AB3EE020206057C3EFB5C8DDE23A432C6F7B92101B93006B0B878C696`。

所有结构化 gate 均为 `true`：

- RTSP 输入解码 2,379 个 HEVC 视频帧，编码 2,549 个 H.264 视频帧；恢复闸门丢弃
  41 个不安全 access unit，音频丢帧为零。
- 外部探针收到 3,117 个 H.264 视频 packet 与 4,911 个 AAC packet；首视频 packet
  为 keyframe。
- 视频 PTS/DTS、音频 PTS/DTS 和 PCR 回退均为零；视频最大间隔 3,000/90kHz，PCR
  最大间隔 900,000/27MHz，均为 33.3ms。
- 20 秒时停止发布源 5 秒，RTSP 观察到断线并完成一次重连；恢复后等待安全 IRAP。
- 50 秒时注入 40ms RTT、20ms 抖动和 1% 丢包 10 秒；SRT 发生重传，探针最终 TS
  continuity error、discontinuity 和 corrupt unit 均为零。
- 引擎处理延迟 p95 为 107ms；RSS 增长 384KiB。
- GPU surface 高水位 3/4；每条运行时队列的高水位均不超过其固定容量 1。
- 运行镜像使用非 root 用户，且没有 `ffmpeg`、`ffprobe` 或 `libav*` 运行时依赖。

探针使用显式 `--latency-ms 240`。该值是外部 SRT 接收恢复窗口，不计入引擎报告的
107ms p95；它用于覆盖本门禁故意注入的网络损伤。

## 门禁中定位并修复的问题

1. RTP relay 没有保留库层的 random-access 标记，运行时会一直等待恢复点。RTSP
   adapter 现在还从 Annex-B NAL header 识别 H.264 IDR type 5 和 HEVC IRAP type
   16—23；它仍不会把任意帧当成安全恢复点。
2. NVDEC parser 处理参数或预热 access unit 后可能没有 display callback。旧代码把
   空 display queue 误判为“已有帧但缺少 format”；现在只有真实 display callback
   出现时才要求 sequence format。
3. Retina `FrameFormat::SIMPLE` 会剥离帧内 VPS/SPS/PPS，并只在它自己判定关键帧时从
   SDP 重新插入。aimedia 的 Annex-B 兜底判断发生得更晚，因此 NVDEC 曾收到没有参数集
   的 IRAP。adapter 现在在“库未标记、但 Annex-B 确认为恢复点”时，从当前
   `VideoParameters.extra_data()` 前置规范化参数集。
4. SRT 诊断偶尔写在 pretty JSON 之后，旧 PowerShell 包装器会把整段日志交给
   `ConvertFrom-Json`。脚本现在只提取第一个完整 JSON 对象，并把其余行保存在
   `probeDiagnostics`。
5. 旧脚本把 `latency=20000` 写在 URI query 中，但 SRT adapter 只从 `SrtConfig` 读取
   latency，该 query 实际被忽略。`aimedia probe` 现在提供有范围校验的
   `--latency-ms`，报告也记录实际配置值。

## 仍未覆盖

- 两台不同厂商的物理 H.265 摄像机或两个 ONVIF 合规设备。
- 两小时连续运行与 Retina keepalive 长时行为。
- 本次 HEVC 源到 RTMP/RTMPS 输出的独立组合门禁；H.264/AAC RTMP publisher 本身已有
  V3-03 外部证据，但不能据此冒充这个具体组合已验证。
- H.265 Main10、HDR、隔行、4:2:2/4:4:4、HEVC 输出、Enhanced RTMP 和 SRT/TS HEVC。

因此 RTSP 整体和 HEVC 输入桥接都保持 `experimental`；物理设备认证与长稳完成前不
升级为 `supported`。
