# RTMP 外部互操作与故障门禁报告

## 结论

2026-08-06 的 180 秒门禁和后续 420 秒故障回归验证了以下真实链路：

`FFmpeg RTMP -> aimedia GPU -> aimedia RTMP -> MediaMTX -> FFmpeg/ffprobe`

测试包含输入 publisher 中断、输出 MediaMTX 中断，以及 40ms RTT、20ms 抖动、1%
丢包。全部自动 gate 通过。420 秒回归还跨过了多个 RTMP ACK 窗口，输入和输出各只有
一次计划内重连。它证明 FFmpeg、MediaMTX 和明文 RTMP 的短时互操作及故障恢复，不替代
OBS、RTMPS 真实平台或两小时稳定性验证，因此状态仍是 `experimental`。

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

### 420 秒 ACK 与重连回归

修复后镜像 `sha256:ea041dba0836edd9d9d6774553ee49f5a55579cf185df793f022936f42b3435d`
通过 420 秒回归。输入在 60 秒中断 8 秒，输出在 300 秒中断 8 秒，350—370 秒注入
40ms RTT、20ms 抖动和 1% 丢包：

| 检查项 | 结果 |
|---|---:|
| 输入重连 | 1 次，与计划故障一致 |
| 输出重连 | 1 次，与计划故障一致；无周期性额外重连 |
| 引擎延迟 | p50 67ms；p95 141ms；max 250ms |
| 输入/输出包 | 31,402 / 31,250 |
| 视频/音频编码帧 | 12,466 / 19,526 |
| 基线与恢复 | H.264/AAC，PTS 单调，首视频包均为 keyframe |
| RSS 变化 | +24 MiB，低于 64 MiB 短测门槛 |
| 隔离拓扑设备显存 | 178 -> 180 MiB，增长 2 MiB |
| 队列与 surface | 队列最高 1/1；NVDEC surface 最高 3/4 |

本机完整汇总位于
`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtmp-4a25bf85\summary.json`。临时目录不是
长期发布存储；SHA-256 为
`E3E4348857BF4DF4DD4B4040E06E8D73AD6B23CA3B229991BC1D0021EF7C6A5A`。V3-03F4
完成时仍需保存两小时原始证据。

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
5. 已建立的 publisher 会话曾把协议终止映射为不可恢复的处理错误，输出接收端故障会
   终止整个管线。现在建立后的协议/TCP 故障统一进入有界输出重连；握手和配置错误仍在
   建连阶段返回稳定错误。
6. `shiguredo_rtmp 2026.1.0-canary.6` 在累计发送约 2.5 MB 时可能先于对端 ACK 的网络
   往返进入 `Disconnecting`。pcap 证明 MediaMTX 已返回序号 `2,500,256` 的 ACK，但
   依赖发送侧在同一边界先判定超时。aimedia 精确固定
   [`anvsk/rtmp-rs@00e97a6`](https://github.com/anvsk/rtmp-rs/commit/00e97a651d0a08a5b7e4837cc2ad8b4701bc2e9a)：
   对齐 peer bandwidth 窗口，并允许宿主关闭依赖内部的 missing-ACK 主动断连。ACK 仍然
   正常收发，真正的故障继续由有界缓冲、TCP 写超时和 aimedia 重连状态机处理。
7. 网络损伤时 ffprobe 会把 FLV 警告写到 stderr；PowerShell Docker 包装器合并输出后
   导致 JSON 解析失败。门禁改用 `ffprobe -v quiet`，状态查询也增加有限重试与明确诊断。

## 原始证据

本机原始产物目录：

`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtmp-646a3976`

- `summary.json` SHA-256：
  `4818e1f958520ca1aff5e23f74f13545177128a223ff7a193c1fd7e18d646ef5`
- `samples.jsonl` SHA-256：
  `404567cc8b909214f86f38dd3d9ffb15261455eb9482d3d5e3d7ac7f053a5154`

临时目录不是长期发布存储。两小时门禁完成时，应将原始 summary 和 samples 作为
Release 附件保存并在报告中记录下载地址与哈希。

2026-08-06 另启动了一次两小时门禁，按用户要求在 2,911 秒（98 个 30 秒样本）主动
停止并清理容器，因此没有 `summary.json`，不能算作 V3-03F4 通过。停止前输入/输出均
连接，重连计数分别为 1/1，延迟 p95 72ms，RSS 为 240,205,824 bytes，隔离设备显存
180 MiB，队列最高 1/1，surface 最高 3/4。部分证据位于
`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtmp-12e42279`；`samples.jsonl` 的 SHA-256
为 `E61989631B192B7B3EED8FC6C4B6BF0A1968736A4B234C6B5644C269BB2333E9`。

## 尚未完成

- OBS 作为 RTMP publisher 和 consumer 的真实互操作；
- 至少两个真实直播平台 endpoint，包括 RTMPS 证书与鉴权错误路径；
- 1080p30 两小时 GPU soak；
- 上述门槛完成前不把 RTMP/RTMPS 从 `experimental` 升为 `supported`。
