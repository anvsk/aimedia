# 暂停交接：v0.3 Normalize & Bridge

更新时间：2026-08-10

暂停点：V3-03F2 的 OBS RTMP 发布、接收和实际渲染门槛完成后

Git 基线：`main` 的 `6b86a7b`，交付分支 `codex/feat/rtmp-obs`

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
| RTMP FFmpeg -> aimedia -> MediaMTX 180/420 秒故障门禁 | 本交付 | PR #38；`docs/reports/rtmp.md` |
| RTMP publisher ACK 竞态修复 | 本交付 | PR #38；`anvsk/rtmp-rs@00e97a6` |
| OBS RTMP publisher/consumer 与实际渲染 | 本交付 | PR #39；`tools/interop.ps1 -Suite rtmp-obs`；`docs/reports/rtmp.md` |
| RTMP 真实平台、完整两小时 soak | 未完成 | 保持 `experimental` |
| v0.4 多输出与 AI Tap | 未开始 | 暂停期间不要开发 |

V3-03F 短门禁在 RTX 5060 Laptop + 577.12 上运行 180 秒。修复 ACK 竞态后又运行
420 秒：输入和输出各只有一次计划内重连，网络损伤为 40ms RTT、20ms 抖动、1% 丢包；
p95 141ms，音视频 PTS 单调，恢复片段首视频包为 keyframe，队列最高 1/1、surface
最高 3/4，运行镜像无 FFmpeg/libav。

V3-03F2 使用 OBS 30.0.2.1 完成两条 20 秒独立链路。OBS publisher 经 aimedia GPU
转码和 MediaMTX 后，ffprobe 读到 581 个 H.264 视频包与 948 个 AAC 音频包，首视频包
为 keyframe，PTS/DTS 单调。反向链路由 OBS Media Source 实际渲染 1280x720 彩条，
PNG 解码后检测到 13 种颜色；OBS、aimedia 均正常退出。运行镜像再次确认没有
FFmpeg/ffprobe 或 `libav*`。

两小时测试使用相同镜像启动，按用户要求在 2,911 秒主动停止，容器与网络已清理。停止
前输入/输出仍连接，重连 1/1，p95 72ms，RSS 约 229 MiB，隔离设备显存 180 MiB；因为
没有跑满且没有生成最终 `summary.json`，V3-03F4 必须保持未完成。

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

1. 用户提供测试直播账号或临时 stream key 后，验证至少两个 endpoint。建议一条国内
   通用 RTMP/RTMPS、一条海外 RTMPS；凭证只通过环境变量或挂载文件传入，日志不得输出。
2. 如果上述互操作带来代码变更，先重新跑 420 秒 ACK 与故障回归；输入/输出重连必须
   仍严格等于计划次数。
3. 用户允许继续长稳测试后，重新跑满 1080p30 两小时门禁并保存 summary/samples 到
   GitHub Release。不要把本次 2,911 秒的中止运行拼接或折算为通过：

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

4. 全部通过后才勾选 V3-03F、更新支持矩阵并考虑 v0.3 Release。

本轮新增的 ACK 修复已经完成，无需下次重新定位：

- 原因一：已建立 publisher 会话的协议终止被映射为 `Processing`，输出故障会结束管线；
  现在建立后的协议/TCP 故障映射为可重连的 `Io`。
- 原因二：固定的 `shiguredo_rtmp` 在约 2.5 MB ACK 边界可能先于 ACK 网络往返进入
  `Disconnecting`。pcap 已看到 MediaMTX 返回 `2,500,256`，但发送侧在同一边界先
  自断。fork [`anvsk/rtmp-rs@00e97a6`](https://github.com/anvsk/rtmp-rs/commit/00e97a651d0a08a5b7e4837cc2ad8b4701bc2e9a)
  对齐窗口并增加默认保持上游行为的配置项；aimedia publisher 关闭依赖内部主动断线，
  仍保留 ACK 收发，并由自身有界缓冲、TCP 超时和重连状态机处理故障。
- 原因三：网络损伤时 ffprobe 的 stderr 警告被 PowerShell 包装器并入 JSON；脚本现在用
  `-v quiet`，控制状态查询增加有限重试和容器状态诊断。

420 秒汇总：

`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtmp-4a25bf85\summary.json`

SHA-256：`E3E4348857BF4DF4DD4B4040E06E8D73AD6B23CA3B229991BC1D0021EF7C6A5A`

主动停止的 2,911 秒部分样本：

`C:\Users\anvsk\AppData\Local\Temp\aimedia-rtmp-12e42279\samples.jsonl`

SHA-256：`E61989631B192B7B3EED8FC6C4B6BF0A1968736A4B234C6B5644C269BB2333E9`

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
docker build -f docker/Dockerfile.gpu --target sdk-runtime -t aimedia:rtmp-next .
pwsh ./tools/rtmp.ps1 -EngineImage aimedia:rtmp-next -DurationSeconds 420 `
  -InputFaultAtSeconds 60 -InputFaultSeconds 8 `
  -OutputFaultAtSeconds 300 -OutputFaultSeconds 8 `
  -ImpairAtSeconds 350 -ImpairSeconds 20
```

提交前还要在 Linux builder 跑 workspace tests、严格 Clippy、release build，并等待 GitHub
的 Rust、NVIDIA ABI、supply-chain 和 fuzz checks 全绿。

## 暂停原则

本交付合并后按用户要求停止开发，不创建 v0.4 分支，不继续后台测试，也不发布“RTMP
已 supported”的社区广告。下次恢复时依次完成 V3-03F3 真实平台和 V3-03F4 完整
两小时长稳；优先补外部证据，不继续增加新协议或格式。
