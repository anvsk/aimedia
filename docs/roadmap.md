# aimedia 执行路线图

`aimedia` 的目标是替代开发者在实时媒体服务中常见的 FFmpeg 子进程、Shell 拼接和
外部守护逻辑：协议接入、格式归一化、转码、多路发布、故障恢复、监控和 AI 分析接入。

本文件同时是产品路线图和执行看板。完成项使用 `[x]`；只有代码已合并并存在对应验证
证据才能勾选。接口骨架、mock 或计划文档不能替代真实数据面证据。

## 自主执行规则

1. 始终推进当前最早且前置条件已具备的工程切片，不因新协议更有趣而跳过技术、兼容
   或发布门槛。社区帖子若因账号资格或审核暂未公开，继续单独追踪且不得伪装完成，
   但在代码、验证和 Release 已完成后不阻塞下一阶段工程。
2. 每个技术切片从最新 `main` 创建功能分支，通过独立 PR、CI 和 code review 合并。
3. 完成切片的同一个 PR 更新本路线图，写明 PR、测试或兼容证据。
4. 小切片不要求用户逐次确认；只有许可证、外部账号、产品取舍或破坏性操作才暂停询问。
5. 每个大阶段完成后创建版本 tag/GitHub Release，并更新支持矩阵和性能报告。
6. 每个大阶段只发布真实、可复现的社区进展：优先 Hacker News、相关 Reddit 社区和
   Rust/音视频社区；不重复灌水、不刷评论或经验值、不规避社区过滤。
7. 社区反馈必须回写用户故事、兼容矩阵或下一阶段排序，不能只统计浏览量。

## 总览

| 阶段 | 状态 | 用户得到什么 | 发布与社区门槛 |
|---|---|---|---|
| v0.1 Foundation | ✅ 完成 | 可测试的媒体基础、fake runtime 和 native adapter | 已进入公开仓库；不宣称真实直播闭环 |
| Architecture Reset | ✅ 完成 | 通用媒体作业定位、类型化执行图、导播降为可选策略 | PR [#10](https://github.com/anvsk/aimedia/pull/10)；属于内部架构切片，不单独广告 |
| v0.2 Native Live Pipe | 🟨 工程与发布完成，社区待公开 | 一路 SRT 真正原生转码后输出 SRT | tag `v0.2-native-live-pipe` 已发布；V2-11 继续追踪 |
| v0.3 Normalize & Bridge | 🚧 进行中 | RTSP/SRT/RTMP 输入归一化并发布 SRT/RTMPS | tag `v0.3-normalize-bridge`；中外平台实测帖 |
| v0.4 Fan-out & AI Tap | ⏳ 未开始 | 一次解码、多路输出、AI 非阻塞接入 | tag `v0.4-ai-tap`；AI SDK 示例发布 |
| v0.5 Media Job Service | ⏳ 未开始 | API 管理多作业及 GPU 资源 | tag `v0.5-job-service`；部署与运维案例 |
| v0.6 Regional Profiles | ⏳ 未开始 | 中国大陆与海外主流工作流预设 | 分区域兼容报告，不做未验证平台广告 |
| v0.7 Extension SDK & Beta | ⏳ 未开始 | 稳定扩展 ABI、24 小时运行和升级策略 | Beta Release、完整技术文章和社区发布 |

## v0.1 Foundation

- [x] 严格配置、密钥引用和版本化本机控制协议。
- [x] 流式 MPEG-TS demux/mux、H.264 Annex-B 和 AAC ADTS。
- [x] libsrt adapter、重连状态、独立节目时钟和有界队列。
- [x] libxaac 帧级 adapter、native round-trip 和固定音频采样时间线。
- [x] NVIDIA SDK 探测和 GPU surface 所有权契约。
- [x] fake backend 单路调度、导播策略、音频 DSP、replay、bench 和 fuzz。

这些完成项证明基础模块行为，不证明真实 GPU 媒体闭环。

## v0.2 Native Live Pipe

目标工作流：

```text
1x SRT/MPEG-TS -> NVDEC + AAC decode -> program timeline
               -> NVENC + AAC encode -> MPEG-TS/SRT
```

### PR 执行清单

- [x] **V2-01 架构与目录重置**：短目录、`aimedia-graph`、新产品定位和 RFC。
  证据：PR [#10](https://github.com/anvsk/aimedia/pull/10)，Linux tests、Clippy、Docker
  build/smoke 和 GitHub CI 全绿。
- [x] **V2-02 执行计划接管运行时**：`SinglePipeline` 保存编译后的 `ExecutionPlan`，
  从计划读取队列与资源约束；配置、`explain` 和运行时不再各维护一套拓扑。
  证据：PR [#11](https://github.com/anvsk/aimedia/pull/11)，计划/运行时差分测试、Linux
  workspace、严格 Clippy、依赖许可证和 Docker build/smoke 全绿。
- [x] **V2-03 NVDEC 帧级后端**：H.264 access unit 输入、格式回调、leased NV12 surface、
  discontinuity reset、IDR 重新同步和 GPU 错误映射。
  证据：PR [#12](https://github.com/anvsk/aimedia/pull/12)，固定 SDK 13.0 ABI headers
  的 feature test/严格 Clippy、workspace/fuzz CI 全绿；RTX 5060 Laptop + 577.12 驱动
  通过真实 IDR 解码、NV12 map/unmap、延迟到下一 IDR 的代际重建及跨代 lease 释放。
- [x] **V2-04 NVENC 帧级后端**：持久输入 surface、H.264 Main/CBR/无 B 帧/1 秒 GOP、
  强制 IDR、SPS/PPS 和 flush。
  证据：PR [#13](https://github.com/anvsk/aimedia/pull/13)，固定 SDK 13.0 ABI headers
  的 feature test/严格 Clippy、workspace/fuzz CI 和 Docker `sdk-build-test` 全绿；
  RTX 5060 Laptop + 577.12 驱动通过 NVDEC NV12 显存帧到持久 NVENC surface 的 GPU
  内复制、首次及显式强制 IDR、Annex-B/SPS/PPS 输出、EOS flush，并将编码结果重新交给
  新 NVDEC 实例成功解码。生产运行时接线与长稳仍属于 V2-05 以后。
- [x] **V2-05 生产后端装配**：把 libsrt、MPEG-TS、NVDEC/NVENC 和 libxaac 注入单路
  调度器；`aimedia run` 不再返回 `nativeVideoBackendPending`。
  证据：PR [#14](https://github.com/anvsk/aimedia/pull/14)，Linux workspace 32 tests 和
  严格 Clippy 全绿；RTX 5060 Laptop + 577.12 驱动完成 15 秒 FFmpeg SRT 输入、原生
  解码/重编码、SRT 输出和 ffprobe 验证，447 个视频包与 694 个音频包 PTS/DTS 无
  倒退，视频 PTS=DTS；运行镜像无 FFmpeg/libav。
- [x] **V2-06 断流与输出恢复**：输入断线输出最后健康帧和静音，恢复后等待 IDR；输出
  断线不积压历史包，重连后重新发送 PAT/PMT 并请求 IDR。
  证据：PR [#15](https://github.com/anvsk/aimedia/pull/15) 的 RTX 5060 Laptop + 577.12
  故障实验中，两段 4 秒输入之间断流时节目
  连续输出；最终视频/音频分别为 37.90/38.08 秒，PTS/DTS 无倒退。输出接收端移除时
  管线继续运行并显式丢弃 608 个视频包和 951 个音频包，输出队列恢复为 0；接收端
  重建后首个视频包为 IDR，PAT/PMT 可立即探测且 TS continuity error 为 0。
- [x] **V2-07 计划与运行状态一致性**：状态报告每条计划边的容量/水位、codec 帧数、
  surface、重连和丢帧；差分测试防止 `explain` 与真实执行器漂移。
  证据：PR [#16](https://github.com/anvsk/aimedia/pull/16) 的计划/运行时差分测试、
  Linux workspace 测试和 NVIDIA feature
  严格 Clippy 全绿；RTX 5060 Laptop + 577.12 实测中 12 条计划边全部可查，
  视频/音频时间线深度与共享物理队列一致；输入恢复后 `reconnects=1`，
  输出断开/恢复分别报告 `connected=false/true` 且 `reconnects=1`。
- [x] **V2-08 外部互操作**：OBS 与 FFmpeg 作为输入端，VLC、OBS、ffprobe 作为输出端；
  caller/listener、损坏 TS、1% 丢包、20ms 抖动和 40ms RTT。
  证据：PR [#17](https://github.com/anvsk/aimedia/pull/17) 的可重复验收脚本与
  [互操作报告](reports/interop.md)。FFmpeg 8.1.2
  caller/listener 四种组合、损坏 TS 和网络损伤通过；OBS 作为真实输入和实际渲染
  输出通过；VLC 3.0.20 raw dump 后由 ffprobe 验证 H.264/AAC、首包 keyframe 与
  PTS/DTS 单调。OBS 本机工具镜像的 teardown crash 与延迟 output listener 触发的
  NVDEC map 失败均已作为独立已知问题保留，没有隐藏在“通过”结论中。
- [x] **V2-09 性能与稳定性**：1080p30 两小时，新增延迟 p95 ≤ 180ms，PTS/DTS/PCR
  单调，RSS/GPU 内存不持续增长，运行镜像无 FFmpeg/libav；修复输入 caller 已积压时
  延迟 output listener 连接导致的 `cuvidMapVideoFrame64` 205，并加入 surface 生命周期
  与延迟接收端回归。
  证据：PR [#19](https://github.com/anvsk/aimedia/pull/19) 与
  [性能和稳定性报告](reports/v0.2-native-live-pipe.md)。RTX 5060 Laptop +
  577.12 驱动连续运行 7,215 秒，216,017 个运行时视频样本的 p50/p95 为
  134/173ms；视频/音频零丢帧，PTS/DTS/PCR 零倒退，surface 高水位 3/4，RSS
  首尾窗口 +2.5MiB，设备显存 178→178MiB，运行镜像无 FFmpeg/libav。延迟 4 秒连接
  output listener 的回归同样零丢帧且未再出现 NVDEC 205。
- [x] **V2-10 发布**：更新支持矩阵和 `docs/reports/v0.2-native-live-pipe.md`，创建 tag、
  GitHub Release、可复现实验命令和演示素材。证据：PR
  [#20](https://github.com/anvsk/aimedia/pull/20)、tag `v0.2-native-live-pipe` 和
  [GitHub prerelease](https://github.com/anvsk/aimedia/releases/tag/v0.2-native-live-pipe)；
  Release 附件包含两小时 summary 与逐 30 秒采样，GitHub 记录的 SHA-256 与报告一致。
- [ ] **V2-11 社区发布**：发布 Hacker News 与一个最相关 Reddit 社区，清楚列出支持与
  pending；记录链接、反馈主题和路线图调整，不通过灌水获取社区权限。2026-08-05
  首轮结果见[社区发布记录](community-feedback.md)：r/rust 已生成帖子但未进入 `/new`，
  Hacker News 在创建 story 前触发 Show HN 新用户限制；当前无真实反馈，因此保持未完成。

V2-01 至 V2-10 构成 v0.2 工程与 Release 门槛，已经完成。V2-11 仍是必须履行的公开
传播义务，但它依赖社区审核和账号资格；保持未完成并定期复查，不再让它无限阻塞
v0.3 工程。只有 V2-11 也完成后，总览才把 v0.2 标记为完全完成。

## v0.3 Normalize & Bridge

用户故事：直播后端开发者把现场 RTSP/SRT/RTMP 输入归一化后发布到国内外平台。

- [x] **V3-01 MediaJob v2 配置**：取代 `DirectorPipeline` 运行时适配层，提供显式转换
  命令；单输出和 director tap 先使用未来列表结构，但未实现的数据面能力会明确拒绝。
  证据：PR [#23](https://github.com/anvsk/aimedia/pull/23)，workspace tests、严格 Clippy、
  Docker explain/conversion smoke、NVIDIA SDK ABI build/test 和更新后验收脚本语法检查。
- [ ] **V3-02 RTSP/RTP 输入**：按 [RFC 0002](rfcs/0002-rtsp-input.md) 完成主流摄像机
  拉流；H.264 + AAC/G.711 形成完整输出闭环，H.265 完成 depacketize 后交给 V3-04。
  - [x] V3-02A：契约、依赖审查、MediaJob RTSP schema 与 fixture 语料。
    证据：PR [#26](https://github.com/anvsk/aimedia/pull/26)，严格 Clippy、core/graph
    12 项测试、RTSP/SRT 字段隔离与 H.264/AAC、H.265/G.711、损坏 SDP fixtures。
  - [x] V3-02B：短目录 `crates/rtsp` 的会话、鉴权、SDP 和类型化媒体事件。
    证据：PR [#27](https://github.com/anvsk/aimedia/pull/27)，Rust 1.88 workspace 门禁、
    cargo-deny 全项通过、Docker smoke，以及 `DESCRIBE/SETUP/PLAY/RTP/TEARDOWN`
    本机伪摄像机闭环。
  - [x] V3-02C：TCP interleaved H.264/AAC/G.711 单路运行时闭环。
    证据：PR [#28](https://github.com/anvsk/aimedia/pull/28)，RTSP/RTP access unit
    直接进入有界 codec 队列，不伪造 MPEG-TS；本机伪摄像机、G.711 8kHz 单声道到
    48kHz 双声道桥接、RTSP 状态、Linux workspace 43 项测试、严格 Clippy、
    cargo-deny、release Docker build 和 `explain` 图烟雾检查通过。外部摄像机、GPU
    真实闭环和长稳仍由 V3-02F 验收，因此尚不升级为 `supported`。
  - [ ] V3-02D：会话恢复与 UDP 可靠性。
    - [x] V3-02D1：TCP 读超时/断流后按有上限的指数退避重连；重连期间不缓存
      历史媒体，恢复后分别标记视频和音频 discontinuity，媒体规格变化则明确终止。
      证据：PR [#29](https://github.com/anvsk/aimedia/pull/29)，本机伪摄像机两次
      `DESCRIBE/SETUP/PLAY` 闭环和断线恢复状态通过；Linux workspace 44 项测试、
      严格 Clippy、cargo-deny 和 release Docker 构建通过。
    - [x] V3-02D2 产品取舍：UDP RTP/RTCP 与重排不再阻塞 v0.3。固定的
      Retina 0.4.19 会在公开 `PacketItem` 前丢弃乱序 UDP 包；上游 issue #40 仍未完成，
      已有原型对视频的假设不成立且无重传时延迟收益低。当前继续明确拒绝 UDP，
      有真实用户要求或上游成熟方案后再恢复，不为了勾选而维护长期 fork。
  - [x] V3-02E：H.265 RTP 重组、明确 bridge pending 与 V3-04 handoff。
    证据：PR [#30](https://github.com/anvsk/aimedia/pull/30)，两个 RFC 7798 FU RTP 包重组为
    单个 Annex-B IDR access unit 并以 `CodecId::H265` 交给公共媒体边界；native GPU
    运行前仍返回稳定 `videoBridgePending`，Linux workspace 45 项测试与严格 Clippy
    通过。
  - [ ] V3-02F：外部设备、网络损伤、两小时 soak 和支持矩阵升级。
    - [x] V3-02F1：MediaMTX 1.20.0 + FFmpeg 发布端的外部软件互操作、GPU 闭环、
      发布源断开/404 重试/恢复，以及 40ms RTT、20ms 抖动、1% 丢包短门禁。
      证据：`tools/rtsp.ps1` 和 [RTSP 互操作报告](reports/rtsp.md)；180 秒运行 p95
      90ms，PTS/DTS/PCR 零回退，TS 零损坏，队列和 surface 有界。该证据不能冒充
      物理摄像机认证。
    - [ ] V3-02F2：至少两台不同厂商摄像机或两个 ONVIF 合规设备，记录型号、固件、
      H.264/AAC 或 G.711 profile 和鉴权结果。
    - [ ] V3-02F3a：外部软件 RTSP 1080p30 两小时 GPU 门禁和证据报告。
    - [ ] V3-02F3b：完成 V3-02F2 物理设备验证后，再决定是否升级支持矩阵；软件 soak
      不能代替摄像机兼容性。
- [ ] **V3-03 RTMP/RTMPS 与 FLV**：接收编码器发布并向国内外平台推流；不建设观众
  播放、GOP cache 或 CDN。
  - [x] V3-03A：配置契约、[RFC 0003](rfcs/0003-rtmp-flv.md)、pending 执行图、
    `rtmpDataPlanePending` 稳定错误和示例。输入固定 RTMP listener，输出固定
    RTMP/RTMPS publisher；URI 与 stream name 分离，所有队列仍有硬上限。证据：
    [PR #32](https://github.com/anvsk/aimedia/pull/32)。
  - [x] V3-03B：短目录 `crates/rtmp` 的 Sans-I/O listener/publisher 会话适配器；
    `shiguredo_rtmp 2026.1.0-canary.6` 已精确固定并封装在 aimedia 自有类型后。入口在
    协议分配前限制 64 KiB 单次 feed、64 个 chunk stream、8 MiB 默认 message，控制
    发送缓冲限制 1 MiB；已通过任意分块、超大声明、chunk stream spray、流名脱敏、
    断线隔离和明文发布回环。证据：[PR #33](https://github.com/anvsk/aimedia/pull/33)。
  - [x] V3-03C：`crates/rtmp/src/avc.rs` 和 `aac.rs` 完成 H.264 Annex-B/AVCC、
    SPS/PPS、AAC ADTS/raw 和 sequence header 双向转换。输入配置变化后重新等待 IDR，
    输出在媒体前重发配置；只接受 48 kHz 双声道 AAC-LC，并保留 DTS 与 composition
    offset 的边界。
  - [ ] V3-03D：RTMP listener 输入接入单路原生运行时，断开后等待新 publisher、
    sequence header 和 IDR。
  - [ ] V3-03E：RTMP/RTMPS publisher、rustls、输出重连、配置重发和 IDR 闸门。
  - [ ] V3-03F：OBS、FFmpeg、MediaMTX、至少两个真实平台 endpoint 的互操作、网络
    故障和两小时 soak；全部通过后才升级为 `supported`。
- [ ] **V3-04 HEVC Bridge**：H.265 输入转 H.264 输出。
- [ ] **V3-05 格式归一化**：720p/1080p、25/30/50/60fps、横竖屏、44.1/48kHz
  和单/双声道。
- [ ] **V3-06 平台互操作**：腾讯云、阿里云、YouTube 非公开直播和 Twitch bandwidth
  test 真实验证。
- [ ] **V3-07 可诊断连接**：分阶段 DNS/TLS/鉴权/格式错误，敏感信息不进入日志。
- [ ] **V3-08 发布**：两小时跨协议 soak、支持矩阵、版本 Release 和社区兼容报告。

## v0.4 Fan-out & AI Tap

- [ ] 一个输入只解码一次，处理结果可供多个输出复用。
- [ ] 每个输出独立有界、独立重连和独立故障域。
- [ ] 非阻塞视频抽样、PCM、传输指标和时间事件接口。
- [ ] analyzer deadline、采样率、权限和隐私配置。
- [ ] 字幕、审核和精彩片段三个 mock/reference analyzer。
- [ ] AI 变慢十倍或完全离线时，节目帧率和主链延迟不显著变化。
- [ ] 双机位自动导播迁移为官方扩展示例。
- [ ] AI SDK Release、示例演示和社区发布。

## v0.5 Media Job Service

- [ ] `POST /v1/jobs`、查询、更新、停止和事件 API。
- [ ] 多作业 GPU session、显存和带宽准入。
- [ ] 输出配置热更新，尽量不重启输入和共享解码。
- [ ] Prometheus/OpenTelemetry、优雅 drain 和 24 小时 soak。
- [ ] 容器镜像、SBOM、升级策略和生产部署文档。
- [ ] Release、部署案例和运维社区发布。

## v0.6 Regional Profiles

- [ ] 中国大陆：RTSP 摄像机、H.265 输入、腾讯云和阿里云 RTMP/RTMPS。
- [ ] 海外：SRT/RTMP 输入、YouTube/Twitch RTMPS。
- [ ] WHIP 只在至少两个真实服务可测试后进入执行清单。
- [ ] 每个预设记录真实账号、endpoint、codec、GPU、运行时长和限制。
- [ ] 分区域兼容报告和社区发布。

GB28181、厂商专有 ARTC/RTS、RIST、NDI、SDI、HLS CDN、AVS3、DRM 和播放器不进入
当前核心路线。只有真实采用数据足够时才以插件或独立里程碑评估。

## v0.7 Extension SDK & Beta

- [ ] transport、codec 与 GPU 的版本化 native ABI。
- [ ] analyzer、policy 和事件扩展 SDK。
- [ ] VLM adapter、deadline、熔断和隐私控制。
- [ ] 24 小时 soak、故障注入、兼容矩阵和稳定配置升级路径。
- [ ] Beta Release、完整技术文章和社区发布。

## 长期取舍标准

新增功能必须至少满足一项：完成高频市场工作流、减少一次昂贵转码或内存复制、降低
运维故障率、让 AI 安全接入实时主链。仅仅因为 FFmpeg 支持某格式，不构成 aimedia
实现它的理由。
