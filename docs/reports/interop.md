# v0.2 外部互操作报告

本报告只记录真实运行结果。工具镜像与 aimedia 运行镜像分离，FFmpeg、
ffprobe、OBS 和 VLC 不是 aimedia 的运行时依赖。

## 环境

- 日期：2026-08-05
- 宿主：Windows + Linux Docker Desktop
- GPU：RTX 5060 Laptop
- NVIDIA 驱动：577.12
- 引擎镜像：当前 `main` 的 NVIDIA feature release 二进制
- FFmpeg/ffprobe：8.1.2（LinuxServer image digest
  `sha256:38f9befcbb1ceab4980aeb6dccb5b3a9be335094967fd09a0ba3d50882632731`）
- OBS Studio：30.0.2.1，obs-websocket v5
- VLC：3.0.20，额外安装 Ubuntu 的 `vlc-plugin-access-extra` 以提供 SRT access module

## SRT caller/listener 矩阵

命令：

```powershell
pwsh ./tools/interop.ps1 `
  -EngineImage aimedia:state `
  -PeerImage aimedia:test-tools `
  -DurationSeconds 20 `
  -Suite matrix `
  -SkipToolBuild
```

| 引擎输入 | 引擎输出 | 网络损伤 | 视频包 | 音频包 | 首视频包 | PTS/DTS |
|---|---|---:|---:|---:|---|---|
| listener | listener | 20ms x 2、20ms jitter、1% loss | 621 | 972 | keyframe | 单调 |
| listener | caller | 无 | 620 | 979 | keyframe | 单调 |
| caller | listener | 无 | 618 | 967 | keyframe | 单调 |
| caller | caller | 无 | 630 | 992 | keyframe | 单调 |

四组中 FFmpeg 发送端、aimedia 引擎和 ffprobe 接收端退出码均为 0。
受损链路的独立 20 秒复跑观测到：

- 输入 SRT：RTT 33.755ms，62 个丢失包；
- 输出 SRT：RTT 32.731ms，88 个丢失包、88 个重传包；
- 输出 632 个视频包、992 个音频包，首视频包为关键帧，PTS/DTS 无倒退。

`tc netem` 在两端各配置 20ms 延迟和 20ms 正态抖动，单次 SRT 平滑统计
不保证正好显示 40.000ms；配置和实测值同时保留，不用目标值替换实测值。

## 损坏 MPEG-TS 恢复

测试语料每轮在第 120 个 TS packet 前插入 17 字节错位数据，然后以原始
data packet 而非重复用后的 TS 发送到 SRT probe。5 秒结果：

- 接收 10,006,469 字节；
- 恢复出 771 个媒体 packet；
- 报告 24 次 continuity error；
- 丢弃 85 字节后重新找到 TS 同步边界；
- probe 退出码为 0。

## OBS 输入和输出

脚本使用 obs-websocket v5 在一次性 HOME 中创建场景，不读取宿主机的 OBS 配置。
输入场景由 OBS 将本地彩条和 1.5kHz 音频编码为 H.264/AAC SRT，输出场景由 OBS
Media Source 读取 aimedia SRT 并截取实际渲染帧。

| 角色 | 运行时间 | 结果 |
|---|---:|---|
| OBS 作为输入 | 8s | 输出 300 个视频包、469 个音频包；首视频包为 keyframe；PTS/DTS 单调 |
| OBS 作为输出 | 8s | SRT 输入/输出均连接；渲染出 1,416 字节彩条 PNG；aimedia 退出码 0 |

两条场景的 obs-websocket controller 和 aimedia 均正常退出。当前本机临时桌面工具
镜像中的 OBS 在日志已经报告 `Number of memory leaks: 0` 后，于测试 teardown 阶段
出现 segmentation fault，因此报告中的 `cleanShutdown=false`。这不改变媒体互操作
结果，但也不把测试工具退出质量伪装为通过。仓库中的 `desktop` target 已改用固定
Ubuntu 基础镜像，避免继承 FFmpeg 工具镜像的第二套 libav/libsrt；本机受镜像源下载
速度影响，尚未完成该纯净 target 的全量重建。

## VLC 输出

VLC 3.0.20 的 SRT 输入不在 Ubuntu `vlc` 主包中，必须安装
`vlc-plugin-access-extra`。验收脚本在运行前检查真实插件文件，不再根据
`vlc --version` 推断能力。VLC 使用 raw dump 保存 aimedia 原始 TS，避免 VLC 自己
重新 mux 后的时间戳影响引擎验收：

- VLC、aimedia 退出码均为 0；
- 输入、输出 SRT 状态均为 connected；
- 收到 211 个视频包和 332 个音频包；
- 首视频包为 keyframe，PTS/DTS 单调。

## 已知限制与下一门槛

- 如果输入 caller 已开始积压，而 output listener 超过约 3 秒才连接，曾稳定复现
  `cuvidMapVideoFrame64` 返回 205。输出端在引擎前就绪、或引擎使用 output caller
  主动连接已就绪 listener 时未复现。该问题归入 V2-09 的延迟接收端与 surface
  生命周期恢复门槛，在修复和两小时 soak 前不宣称生产稳定。
- OBS 的 `cleanShutdown=false` 是当前本机测试工具镜像的独立已知问题；它不会改写
  aimedia 的退出码或媒体结果。
- 本报告证明 v0.2 支持范围内的外部互操作，不证明 RTMP、RTSP、H.265、缩放、变帧率
  或双路导播能力。

V2-08 的外部系统和网络损伤验收已经完成；发布仍受 V2-09 性能与稳定性门槛约束。
