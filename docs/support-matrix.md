# 支持矩阵

状态定义：

- **supported**：有真实运行和兼容性验证。
- **foundation**：核心行为已实现，但 native 数据面尚未接通。
- **experimental**：真实实现存在，但平台、设备或稳定性覆盖不足。
- **planned**：接口或路线已定义，尚未实现。
- **out-of-scope**：当前 Alpha 不处理。

| 能力 | 状态 | 说明 |
|---|---|---|
| `aimedia/v1alpha1` 配置 | supported | 严格字段、范围和密钥引用校验 |
| 单路/双路配置契约 | foundation | `inputs` 接受 1–2 路；单路 state、`notApplicable` 和队列/codec/GPU/SRT 指标结构已测 |
| 单路有界媒体调度器 | foundation | fake backend 已验证任意 TS 分块、demux/decode、独立节目时钟、encode/mux/send、任务故障联停和队列排空；生产后端尚未接线 |
| 双机位导播状态机 | supported | replay/bench 可运行 |
| 人工 take/hold | foundation | 状态机、版本化 Unix Socket、CLI 和 mock runtime 已测；待接视频 tick |
| VLM 建议约束 | supported | OpenAI 兼容 client、deadline、expiry、25% 权重 |
| MPEG-TS packet probe | supported | PAT/PMT、PCR、continuity 和损坏包报告 |
| 流式 MPEG-TS demux/mux | foundation | 任意字节分块、PSI/PES、CRC、PTS 回绕和 mux 往返已测；待真实流互操作 |
| H.264 Annex-B / AAC ADTS parser | supported | NAL/frame、关键帧和 Alpha profile header |
| BS.1770/4x true-peak DSP | foundation | 响度、增益、淡化和 limiter 已实现；AAC 帧级 codec 已就绪，待接运行时音频链 |
| SRT caller/listener | foundation | libsrt 1.5.5 adapter、epoll、指数退避和断开后 native 重连已测；输出层不保存历史包，待网络损伤与长稳测试 |
| RTMP/RTMPS + FLV | planned | v0.4 中外共同平台发布基线 |
| RTSP/RTP input | planned | v0.6 中国侧摄像机接入；不代表 GB28181 |
| WHIP output | planned | v0.7 海外低延迟发布；H.264/Opus |
| NVDEC/NVENC | foundation | SDK 13.0 driver probe 和 RAII surface 已实现；帧提交尚未实现 |
| AAC-LC via libxaac | foundation | 48kHz 双声道、128kbps ADTS 帧级 encode/decode、core backend adapter、flush、固定 cadence 和 native round-trip 已测；待接单路调度器与长稳 |
| Silero VAD / 视觉 ONNX | planned | ONNX Runtime adapter |
| REST/WebSocket 控制面 | planned | Beta |
| WHEP、HTTP-FLV/HLS viewer output | out-of-scope | 当前产品不建设观众侧源站或 CDN |
| HEVC、AV1、文件 seek | out-of-scope | 后续阶段 |
| FFmpeg CLI 全兼容 | out-of-scope | 只逐项增加已验证翻译 |

后续状态升级必须记录 producer、consumer、平台或设备、GPU/驱动、连续运行时长和
已知限制。平台名称出现在路线图中不等于已经兼容；真实账号未验证的社交平台保持
`experimental` 或 `planned`。
