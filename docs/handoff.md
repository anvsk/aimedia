# 暂停交接：v0.3 Normalize & Bridge

更新时间：2026-08-06

暂停点：V3-03F 的 FFmpeg + MediaMTX 短时互操作与故障门禁完成后

Git 基线：`main` 的 `cd9af02`，交付分支 `codex/feat/rtmp-interop`

## 先读结论

项目没有改回“双机位导播应用”。当前产品是面向开发者和集成商的原生实时媒体引擎：
接入主流直播协议，归一化 H.264/AAC，使用独立节目时钟和有界执行图完成 GPU 转码，
再发布到平台；不做 CDN、播放器或 FFmpeg 全格式复刻。

已经 `supported` 的市场闭环仍是单路 SRT/MPEG-TS。RTSP 和 RTMP 已有真实 GPU 数据面，
但设备、平台或长稳证据不完整，必须继续标记 `experimental`。暂停后不要进入 v0.4，
应先关闭本文件列出的 v0.3 外部门槛。

## 已交付到哪里

| 范围 | 状态 | 证据 |
|---|---|---|
| MediaJob v2 与通用执行图 | 已合并 | PR #23 |
| RTSP schema、TCP 会话、H.264/AAC/G.711、重连、HEVC access unit | 已合并 | PR #26—#30 |
| RTSP 外部软件短门禁 | 已合并 | PR #31；`docs/reports/rtsp.md` |
| RTMP/FLV 契约、会话、AVC/AAC 转换、listener、RTMP/RTMPS publisher | 已合并 | PR #32—#36 |
| RTMP FFmpeg -> aimedia -> MediaMTX 短门禁 | 本交付 | `docs/reports/rtmp.md` |
| RTMP OBS、真实平台、两小时 soak | 未完成 | 保持 `experimental` |
| v0.4 多输出与 AI Tap | 未开始 | 暂停期间不要开发 |

V3-03F 短门禁在 RTX 5060 Laptop + 577.12 上运行 180 秒。输入和输出各恢复一次，
网络损伤为 40ms RTT、20ms 抖动、1% 丢包；p95 142ms，音视频 PTS 单调，恢复片段首
视频包为 keyframe，所有队列和 GPU surface 未越界，运行镜像无 FFmpeg/libav。

## 当前代码结构

```text
crates/
  core/       公共媒体类型、配置、后端接口
  graph/      把 MediaJob 意图编译为有界执行计划
  runtime/    单路任务、节目时钟、队列、恢复和状态
  mpegts/     TS 拆包与封包
  srt/        libsrt 传输边界
  rtsp/       摄像机 RTSP/RTP 输入边界
  rtmp/       RTMP/RTMPS 会话、FLV AVC/AAC 转换、source/sink
  nvidia/     NVDEC/NVENC 与 surface 生命周期
  aac/        libxaac 编解码边界
  cli/        aimedia 命令入口
tools/        可复现的外部互操作与 soak 门禁
docs/         路线图、RFC、报告和本交接
```

媒体主链是：

```text
SRT/TS | RTSP/RTP | RTMP/FLV
        -> 公共 H.264/AAC packet
        -> NVDEC + libxaac decode
        -> 独立节目时钟与有界队列
        -> NVENC + libxaac encode
        -> TS/SRT | FLV/RTMP(S)
```

输入时间戳只用于映射，输出始终使用独立单调节目时钟。协议 crate 不拥有无界队列；
FFmpeg 和 MediaMTX 只属于测试端，不进入运行镜像。

## 必须先处理的未完成项

### 1. RTMP V3-03F

按以下顺序关闭门槛：

1. 跑 1080p30 两小时门禁并保存 summary/samples 到 GitHub Release：

```powershell
pwsh ./tools/rtmp.ps1 `
  -EngineImage aimedia:rtmp-interop `
  -PeerImage aimedia:test-tools `
  -DurationSeconds 7200 `
  -InputFaultAtSeconds 600 `
  -InputFaultSeconds 12 `
  -OutputFaultAtSeconds 1800 `
  -OutputFaultSeconds 12 `
  -ImpairAtSeconds 3600 `
  -ImpairSeconds 60 `
  -SampleIntervalSeconds 30
```

2. 复用 `tools/obs.py` 的隔离 HOME 和 obs-websocket 方式，增加 OBS RTMP publisher 与
   consumer 门禁；必须检查实际渲染帧和 ffprobe，不只看连接状态。
3. 用户提供测试直播账号或临时 stream key 后，验证至少两个 endpoint。建议一条国内
   通用 RTMP/RTMPS、一条海外 RTMPS；凭证只通过环境变量或挂载文件传入，日志不得输出。
4. 全部通过后才勾选 V3-03F、更新支持矩阵并考虑 v0.3 Release。

### 2. RTSP V3-02F

两小时软件 soak 曾计划运行 7,200 秒，但在约 5,700 秒提前结束，没有生成
`summary.json`，因此不算通过。期间出现 9 次非预期重连，p95 约 109—111ms。

现有证据指向 Retina 0.4.19 的 TCP interleaved 播放循环：连续媒体始终 ready 时，
keepalive timer 可能饥饿，MediaMTX 最终按 read timeout 关闭 reader；配置中的
`keepaliveMs` 当前没有真正控制该行为。下次应先选择并记录一种处理：提交上游修复、
维护最小 fork，或删除/明确拒绝这个误导配置，然后重新跑两小时。还需要两台不同厂商
摄像机或两个 ONVIF 合规设备；软件模拟不能代替设备认证。

失败运行目录仍在：

`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtsp-main-327d9764`

## 已知边界和坑

- `apps/` 是用户独立项目目录，在父仓库中保持未跟踪；不得移动、删除、暂存或提交。
- Windows 本机全 workspace 编译会在 `crates/srt` 的 `libc::sockaddr_in` 处失败；Linux
  x86_64 才是产品目标，完整验证统一在 Docker/Linux 运行。RTMP crate 可在 Windows
  单独测试。
- 当前 RTMP listener 只接受明文 `rtmp://`；RTMPS 仅用于 publisher 输出。
- RTMP 只支持传统 FLV AVC/AAC，不支持 Enhanced RTMP、HEVC、AV1、观众播放或 GOP cache。
- RTSP 只支持 TCP interleaved；UDP、H.265 到 H.264 bridge 尚未完成。
- 不要把 `foundation`、内部回环或 180 秒短门禁写成生产支持。
- 社区账号仍受 Hacker News / Reddit 新账号审核限制，不得刷帖、刷评论或规避过滤。

## 下次恢复步骤

```powershell
git switch main
git pull --ff-only origin main
git status --short
git switch -c codex/feat/<next-slice>
```

确认状态里只有预期改动和未跟踪的 `apps/`。先读 `docs/roadmap.md`、
`docs/support-matrix.md`、`docs/reports/rtmp.md` 和本文件，再从最早的未完成外部门槛继续。
每个切片仍从最新 `main` 建独立 `codex/*` 分支，通过 PR 合并；不要直接提交 `main`。

最低回归命令：

```powershell
cargo fmt --all -- --check
cargo test -p aimedia-rtmp
docker build -f docker/Dockerfile.gpu --target sdk-runtime -t aimedia:rtmp-interop .
pwsh ./tools/rtmp.ps1 -EngineImage aimedia:rtmp-interop -DurationSeconds 180
```

提交前还要在 Linux builder 跑 workspace tests、严格 Clippy、release build，并等待 GitHub
的 Rust、NVIDIA ABI、supply-chain 和 fuzz checks 全绿。

## 暂停原则

本交付合并后停止开发，不创建 v0.4 分支，不发布“RTMP 已 supported”的社区广告。
下次恢复时优先补外部证据，而不是继续增加新协议或格式。
