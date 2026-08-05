# 支持矩阵

状态定义：

- **supported**：有真实运行和兼容性验证。
- **foundation**：核心行为已实现，但还没有形成真实用户数据面。
- **experimental**：真实实现存在，但平台、设备或稳定性覆盖不足。
- **planned**：接口或路线已定义，尚未实现。
- **out-of-scope**：当前 Alpha 不处理。

| 能力 | 状态 | 说明 |
|---|---|---|
| `aimedia/v1alpha1` 配置 | supported | 严格字段、范围和密钥引用校验；`DirectorPipeline` 是待迁移的 v0.1 适配层 |
| 类型化执行图 | supported | `aimedia-graph` 输出媒体、内存、时钟、队列、资源和节点实现状态；单路执行器从计划读取队列契约 |
| 单路/双路配置契约 | foundation | `inputs` 接受 1–2 路；单路 state、`notApplicable` 和队列/codec/GPU/SRT 指标结构已测 |
| 单路有界媒体调度器 | experimental | libsrt、MPEG-TS、NVDEC/NVENC 和 libxaac 已接入 `aimedia run`；RTX 5060 Laptop 上完成 15 秒 FFmpeg→aimedia→ffprobe 数据面，待恢复、互操作和长稳门槛 |
| 双机位导播策略 | supported | replay/bench 可运行；属于可选策略示例，不再是核心发布前提 |
| 人工 take/hold | foundation | 状态机、版本化 Unix Socket、CLI 和 mock runtime 已测；待接视频 tick |
| VLM 建议约束 | supported | OpenAI 兼容 client、deadline、expiry、25% 权重 |
| MPEG-TS packet probe | supported | PAT/PMT、PCR、continuity 和损坏包报告 |
| 流式 MPEG-TS demux/mux | experimental | 任意字节分块、PSI/PES、CRC、PTS 回绕和 mux 往返已测；FFmpeg 输入和 ffprobe 输出完成 15 秒真实流互操作，待更多接收端和损坏流验证 |
| H.264 Annex-B / AAC ADTS parser | supported | NAL/frame、关键帧和 Alpha profile header |
| BS.1770/4x true-peak DSP | foundation | 响度、增益、淡化和 limiter 已实现；AAC 帧级 codec 已就绪，待接运行时音频链 |
| SRT caller/listener | experimental | libsrt 1.5.5 adapter、epoll、指数退避和断开后 native 重连已测；输入 listener、输出 caller 完成真实闭环，待四种组合、网络损伤与长稳测试 |
| RTMP/RTMPS + FLV | planned | v0.3 跨区域平台发布基线 |
| RTSP/RTP input | planned | v0.3 摄像机接入；不代表 GB28181 |
| 多输出与 Analyzer Tap | planned | v0.4；每个支路独立有界并与媒体主链隔离 |
| WHIP output | planned | v0.6 以后按真实服务采用证据评估；H.264/Opus |
| NVDEC | experimental | H.264 parser callback、NV12 map/unmap、代际 surface lease、discontinuity/IDR 闸门已实现；RTX 5060 Laptop、577.12 驱动完成 447 帧实时解码，surface 池与有界队列对齐，待长稳 |
| NVENC | experimental | H.264 Main、CBR、无 B 帧、1 秒 GOP、持久注册 NV12 surface、强制 IDR/SPS/PPS 和 EOS 已实现；同机完成 447 帧实时编码，ffprobe 验证 Main/30fps/PTS=DTS，待长稳 |
| AAC-LC via libxaac | experimental | 48kHz 双声道、128kbps ADTS 帧级 encode/decode、core adapter、flush、固定 cadence 和 native round-trip 已测；真实闭环输出 694 个单调时间戳音频包，待长稳 |
| Silero VAD / 视觉 ONNX | planned | ONNX Runtime adapter |
| REST/WebSocket 作业控制面 | planned | v0.5 Media Job Service |
| WHEP、HTTP-FLV/HLS viewer output | out-of-scope | 当前产品不建设观众侧源站或 CDN |
| HEVC、AV1、文件 seek | out-of-scope | 后续阶段 |
| FFmpeg CLI 全兼容 | out-of-scope | 只逐项增加已验证翻译 |

后续状态升级必须记录 producer、consumer、平台或设备、GPU/驱动、连续运行时长和
已知限制。平台名称出现在路线图中不等于已经兼容；真实账号未验证的社交平台保持
`experimental` 或 `planned`。
