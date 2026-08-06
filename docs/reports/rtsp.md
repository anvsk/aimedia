# RTSP TCP 外部互操作报告

## 结论

2026-08-06 的 180 秒短门禁已经证明以下真实数据面可以工作：

`FFmpeg H.264/AAC 发布 -> MediaMTX RTSP/TCP -> aimedia GPU -> SRT/MPEG-TS -> aimedia probe`

测试包含一次 8 秒发布源中断，以及 20 秒的 40ms RTT、20ms 抖动、1% 丢包。所有自动
gate 通过。它证明外部软件互操作和故障恢复，不等同于物理摄像机认证，也不替代两小时
稳定性门禁，因此 RTSP 状态仍是 `experimental`。

## 可复现环境

| 项目 | 值 |
|---|---|
| GPU | NVIDIA GeForce RTX 5060 Laptop GPU，8151 MiB |
| 驱动 | 577.12 |
| aimedia image | `sha256:971fb2fa43837397e7f1fcdf015d1f4500a596a1901a04a5245562f146838cfe` |
| MediaMTX | 1.20.0，`sha256:86e63af28616d5e5a18540d7b031b6510bd4cbf1a3c7d224f9e2976f02aefbfb` |
| 测试工具 image | `sha256:67a84acef4d12fcc2a224c72c128bdfe80457c35d2f2d4132b7672265a4f5119` |
| 视频 | H.264 Main、1920x1080、30fps、6Mbps、无 B 帧、1 秒 GOP |
| 音频 | AAC-LC、48kHz、双声道、128kbps |

运行命令：

```powershell
pwsh ./tools/rtsp.ps1 `
  -EngineImage aimedia:rtsp-interop `
  -DurationSeconds 180 `
  -FaultAtSeconds 35 `
  -FaultSeconds 8 `
  -ImpairAtSeconds 90 `
  -ImpairSeconds 20 `
  -SampleIntervalSeconds 5
```

脚本使用唯一名称创建并只清理本轮容器和 Docker network。MediaMTX、FFmpeg 和
`tc netem` 只存在于验收拓扑；aimedia 运行镜像中没有 `ffmpeg`、`ffprobe` 或 `libav*`。

## 结果

| 检查项 | 结果 |
|---|---:|
| RTSP 接收包 | 12,889 |
| RTSP 恢复 | 1 次，断开状态可见且恢复成功 |
| 输出视频包 | 5,826 |
| 输出音频包 | 9,136 |
| 引擎延迟 | p50 63ms；p95 90ms；max 123ms |
| 视频 PTS/DTS 回退 | 0 / 0 |
| 音频 PTS/DTS 回退 | 0 / 0 |
| PCR 回退 | 0；最大间隔 900,000 ticks，即 33.3ms |
| TS continuity / corrupt / resync | 0 / 0 / 0 |
| 队列高水位 | 所有队列 1/1，没有越界 |
| NVDEC surface 高水位 | 3/4 |
| RSS 变化 | +20.5 MiB，低于 64 MiB 短测门槛 |
| 运行时依赖检查 | 通过，无 FFmpeg/libav |

网络损伤期间 SRT 发送端记录 735 个丢包并全部重传；输出探针仍解析到连续、单调且从
IDR 开始的节目流。发布源中断期间，节目时钟继续输出最后健康画面和静音，恢复后重新
等待输入 IDR，不拼接新旧 RTP 会话时间戳。

## 外部测试发现并修复的问题

1. MediaMTX 的多轨 `PLAY` 响应可能缺少 `rtptime`。Retina 默认策略会拒绝整个会话；
   aimedia 改用 permissive 初始时间戳策略，在任一轨缺失时将各轨第一包映射到 NPT 0，
   后续仍保留时间戳跳变保护，输出仍使用独立节目时钟。
2. 发布源离线时 MediaMTX 对 `DESCRIBE` 返回 404。原实现把所有 4xx 都判为不可恢复，
   导致运行时退出；现在只在已建立输入的重连循环中把 404 当作暂时离线并继续有上限
   退避，认证失败及其他不可恢复 4xx 仍快速失败。
3. 断流时重复最后画面会让旧输入帧年龄污染“引擎处理延迟”。现在新鲜输入仍测完整
   端到端处理时延，兜底帧从当前节目 tick 计时；输入陈旧程度由 RTSP
   `lastDataAgeMs` 和冻结状态独立报告。

## 尚未完成

- 两台不同厂商物理摄像机或两个 ONVIF 合规设备；
- G.711 外部设备链路；
- RTSP 两小时 1080p30 soak；
- UDP RTP、H.265 到 H.264 GPU bridge；
- 真实设备完成前不把 RTSP 从 `experimental` 升为 `supported`。
