# RTMP 外部互操作与故障门禁报告

## 结论

2026-08-06 的 180 秒门禁验证了以下真实链路：

`FFmpeg RTMP -> aimedia GPU -> aimedia RTMP -> MediaMTX -> FFmpeg/ffprobe`

测试包含输入 publisher 中断、输出 MediaMTX 中断，以及 40ms RTT、20ms 抖动、1%
丢包。全部自动 gate 通过。它证明 FFmpeg、MediaMTX 和明文 RTMP 的短时互操作及故障
恢复，不替代 OBS、RTMPS 真实平台或两小时稳定性验证，因此状态仍是 `experimental`。

## 可复现环境

| 项目 | 值 |
|---|---|
| GPU | NVIDIA GeForce RTX 5060 Laptop GPU，8151 MiB |
| 驱动 | 577.12 |
| aimedia image | `sha256:4a54ccec656f74bd2ed7c581389a6d85c44f7057e26c1a23a97335cab6e4fdd0` |
| MediaMTX | 1.20.0，`sha256:86e63af28616d5e5a18540d7b031b6510bd4cbf1a3c7d224f9e2976f02aefbfb` |
| 测试工具 image | `sha256:67a84acef4d12fcc2a224c72c128bdfe80457c35d2f2d4132b7672265a4f5119` |
| 视频 | H.264 Main、1920x1080、30fps、6Mbps、无 B 帧、1 秒 GOP |
| 音频 | AAC-LC、48kHz、双声道、128kbps |

运行命令：

```powershell
pwsh ./tools/rtmp.ps1 `
  -EngineImage aimedia:rtmp-interop `
  -PeerImage aimedia:test-tools `
  -DurationSeconds 180
```

脚本使用唯一名称创建并只清理本轮容器和 Docker network。FFmpeg、ffprobe、MediaMTX
和 `tc netem` 只存在于验收拓扑；aimedia 运行镜像中没有 FFmpeg 或 `libav*`。

## 结果

| 检查项 | 结果 |
|---|---:|
| RTMP 输入包 | 13,232 |
| RTMP 输出包 | 13,098 |
| 输入恢复 | 1 次；8 秒中断被观察并恢复 |
| 输出恢复 | 1 次；8 秒中断被观察并恢复 |
| 引擎延迟 | p50 138ms；p95 142ms；max 206ms |
| 基线采集 | H.264 431 包；AAC 717 包；首视频包为 keyframe |
| 恢复采集 | H.264 431 包；AAC 717 包；首视频包为 keyframe |
| 时间戳 | 两段音视频均单调；视频 PTS 从 19,333 继续到 94,366 |
| 队列高水位 | 所有队列 1/1，没有越界 |
| NVDEC surface 高水位 | 3/4 |
| RSS 变化 | +23.4 MiB，低于 64 MiB 短测门槛 |
| 隔离拓扑设备显存 | 178 -> 180 MiB，增长 2 MiB |
| 运行时依赖检查 | 通过，无 FFmpeg/libav |

网络损伤在 110—130 秒注入。结束时输入和输出均为连接状态，输出断线期间没有建立
历史媒体队列；重连后从新的 SPS/PPS 与 IDR 恢复，节目时钟没有回退。

## 外部测试发现并修复的问题

1. FFmpeg 在握手后先发送 `Set Chunk Size`，再发送 `connect`。固定的 RTMP 协议库会在
   确认窗口仍为零时生成 ACK；适配器若立刻确认写出，上游会错误判定 ACK 超限并主动
   断开。现在 `Connecting` 阶段暂存少量控制输出，`connect` 建立窗口后再发送。
2. MediaMTX publisher 已连接不代表节目轨已可播放。门禁现在要求真实 ffprobe 读到
   H.264 后才启动采集，消除读端启动竞态。
3. `-copyts` 与固定输出 `-t` 组合会让恢复采集在节目 PTS 已超过时长时立即退出。门禁
   改为按墙钟向 FFmpeg 发送中断，既采满 15 秒又保留原始节目 PTS。
4. RTMP 会话失败现在记录协议阶段、稳定错误类别和不含 stream name 的原因，便于区分
   TCP、握手、命令和媒体阶段问题。

## 原始证据

本机原始产物目录：

`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtmp-646a3976`

- `summary.json` SHA-256：
  `4818e1f958520ca1aff5e23f74f13545177128a223ff7a193c1fd7e18d646ef5`
- `samples.jsonl` SHA-256：
  `404567cc8b909214f86f38dd3d9ffb15261455eb9482d3d5e3d7ac7f053a5154`

临时目录不是长期发布存储。两小时门禁完成时，应将原始 summary 和 samples 作为
Release 附件保存并在报告中记录下载地址与哈希。

## 尚未完成

- OBS 作为 RTMP publisher 和 consumer 的真实互操作；
- 至少两个真实直播平台 endpoint，包括 RTMPS 证书与鉴权错误路径；
- 1080p30 两小时 GPU soak；
- 上述门槛完成前不把 RTMP/RTMPS 从 `experimental` 升为 `supported`。
