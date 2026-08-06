# 支持矩阵

状态定义：

- **supported**：有真实运行和兼容性验证。
- **foundation**：核心行为已实现，但还没有形成真实用户数据面。
- **experimental**：真实实现存在，但平台、设备或稳定性覆盖不足。
- **planned**：接口或路线已定义，尚未实现。
- **out-of-scope**：当前 Alpha 不处理。

| 能力 | 状态 | 说明 |
|---|---|---|
| `aimedia/v1alpha2` `MediaJob` 配置 | supported | 以 `inputs`、`processing`、`outputs`、`taps` 和故障策略表达目标；严格字段、范围和密钥引用校验；当前只允许一个输出 |
| `aimedia/v1alpha1` `DirectorPipeline` 转换 | supported | `aimedia config convert` 显式生成新 YAML；`run`/`explain` 不静默解析旧配置 |
| 类型化执行图 | supported | `aimedia-graph` 输出媒体、内存、时钟、队列、资源和节点实现状态；单路执行器从计划读取队列契约，状态以 `from`/`to` 报告每条计划边并对共享物理队列复用同一水位 |
| 单路/双路配置契约 | foundation | `inputs` 接受 1–2 路；单路 `state` 已接入真实队列、codec 帧/丢帧、NVDEC surface 和输入/输出 SRT 重连指标；双路真实数据面仍待 v0.4 后的策略扩展 |
| 单路有界媒体调度器 | supported | libsrt、MPEG-TS、NVDEC/NVENC 和 libxaac 已接入 `aimedia run`；RTX 5060 Laptop 上完成断流恢复、FFmpeg/OBS/VLC 互操作、网络损伤和 1080p30 两小时门禁，p95 173ms、零丢帧 |
| 双机位导播策略 | supported | replay/bench 可运行；属于可选策略示例，不再是核心发布前提 |
| 人工 take/hold | foundation | 状态机、版本化 Unix Socket、CLI 和 mock runtime 已测；待接视频 tick |
| VLM 建议约束 | supported | OpenAI 兼容 client、deadline、expiry、25% 权重 |
| MPEG-TS packet probe | supported | PAT/PMT、PCR、continuity 和损坏包报告 |
| 流式 MPEG-TS demux/mux | supported | 任意字节分块、PSI/PES、CRC、PTS 回绕和 mux 往返已测；FFmpeg/OBS 输入、ffprobe/VLC/OBS 输出、损坏流恢复及两小时 PTS/DTS/PCR 零倒退已验证 |
| H.264 Annex-B / AAC ADTS parser | supported | NAL/frame、关键帧和 Alpha profile header |
| BS.1770/4x true-peak DSP | foundation | 响度、增益、淡化和 limiter 已实现；AAC 帧级 codec 已就绪，待接运行时音频链 |
| SRT caller/listener | supported | libsrt 1.5.5 adapter、epoll、指数退避和断开后 native 重连已测；caller/listener 四种组合、断流恢复、1% 丢包/20ms 抖动/40ms RTT 及两小时长稳已验证 |
| RTMP listener input + FLV | experimental | 明文 `rtmp://` listener、H.264 Annex-B/AVCC 与 AAC ADTS/raw 转换已接入单路 `aimedia run`。FFmpeg 8.1.2 -> aimedia -> MediaMTX 的 180 秒门禁和 420 秒故障回归已通过；后者输入/输出重连严格为计划内 1/1。已修复握手前控制输出和长连接 ACK 竞态。OBS、真实硬件编码器和两小时 soak 未完成 |
| RTMP/RTMPS publisher output | experimental | 编码后的 H.264 Annex-B/AAC ADTS 直接进入有界 publisher；RTMPS 使用 rustls 与公开 WebPKI 信任根校验证书和主机名。MediaMTX 回归验证了输出断线、无历史积压、重连后配置/IDR、节目 PTS 连续和 p95 141ms；OBS、至少两个真实平台 endpoint、RTMPS 实站和两小时 soak 未完成，不能标记为 supported |
| RTSP/RTP input | experimental | `aimedia run` 已接 TCP interleaved 单路数据面：H.264/AAC-LC/G.711 直接进入有界 codec 队列，G.711 8kHz 单声道归一到 48kHz 双声道；MediaMTX 1.20.0 + FFmpeg 发布端已通过 180 秒 GPU 闭环、发布源 404 重连和 40ms RTT/20ms 抖动/1% 丢包短门禁，p95 90ms 且输出时间戳零回退。H.265 可重组为 Annex-B access unit，但 HEVC bridge 仍返回 `videoBridgePending`；UDP、非 48kHz 双声道 AAC、物理摄像机认证和两小时长稳未完成，不代表 GB28181 支持 |
| 多输出与 Analyzer Tap | planned | v0.4；每个支路独立有界并与媒体主链隔离 |
| WHIP output | planned | v0.6 以后按真实服务采用证据评估；H.264/Opus |
| NVDEC | supported | H.264 parser callback、NV12 map/unmap、代际 surface lease、discontinuity/IDR 闸门已实现；RTX 5060 Laptop、577.12 驱动完成两小时真实解码，零丢帧、surface 高水位 3/4 |
| NVENC | supported | H.264 Main、CBR、无 B 帧、1 秒 GOP、持久注册 NV12 surface、强制 IDR/SPS/PPS 和 EOS 已实现；同机两小时输出 216,423 个视频 packet，PTS=DTS 且零倒退 |
| AAC-LC via libxaac | supported | 48kHz 双声道、128kbps ADTS 帧级 encode/decode、core adapter、flush 和固定 cadence 已测；两小时输出 338,163 个音频 packet，PTS/DTS 零倒退 |
| Silero VAD / 视觉 ONNX | planned | ONNX Runtime adapter |
| REST/WebSocket 作业控制面 | planned | v0.5 Media Job Service |
| WHEP、HTTP-FLV/HLS viewer output | out-of-scope | 当前产品不建设观众侧源站或 CDN |
| HEVC、AV1、文件 seek | out-of-scope | 后续阶段 |
| FFmpeg CLI 全兼容 | out-of-scope | 只逐项增加已验证翻译 |

后续状态升级必须记录 producer、consumer、平台或设备、GPU/驱动、连续运行时长和
已知限制。平台名称出现在路线图中不等于已经兼容；真实账号未验证的社交平台保持
`experimental` 或 `planned`。
