# 支持矩阵

状态定义：

- **supported**：有真实运行和兼容性验证。
- **foundation**：核心行为已实现，但 native 数据面尚未接通。
- **planned**：接口或路线已定义，尚未实现。
- **out-of-scope**：当前 Alpha 不处理。

| 能力 | 状态 | 说明 |
|---|---|---|
| `aimedia/v1alpha1` 配置 | supported | 严格字段、范围和密钥引用校验 |
| 双机位导播状态机 | supported | replay/bench 可运行 |
| 人工 take/hold | supported | 核心 API 和 replay command |
| VLM 建议约束 | supported | OpenAI 兼容 client、deadline、expiry、25% 权重 |
| MPEG-TS packet probe | supported | PAT/PMT 单包 section、PCR、continuity |
| H.264 Annex-B / AAC ADTS parser | supported | NAL/frame 边界与关键 header，尚未做 access-unit/PES 重组 |
| 跨包 PSI/PES reassembly | planned | native demux 阶段 |
| BS.1770 风格滚动响度 | foundation | 已有 DSP；未接 AAC codec |
| SRT caller/listener | planned | 必须使用 `libsrt` adapter |
| NVDEC/NVENC | planned | Linux NVIDIA 首发后端 |
| AAC-LC via libxaac | planned | 48kHz stereo Alpha profile |
| Silero VAD / 视觉 ONNX | planned | ONNX Runtime adapter |
| REST/WebSocket 控制面 | planned | Beta |
| RTMP、WHIP/WHEP | out-of-scope | 后续阶段 |
| HEVC、AV1、文件 seek | out-of-scope | 后续阶段 |
| FFmpeg CLI 全兼容 | out-of-scope | 只逐项增加已验证翻译 |
