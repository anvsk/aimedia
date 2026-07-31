# 实施路线图

路线图只记录已经验证的能力和带验收门槛的下一步，不把接口骨架描述为可用后端。
当前目标不是覆盖 FFmpeg 的全部格式，而是在双机位 SRT 节目制作这一条链路中建立可验证的替代方案。

## 当前基线：0.1.0-alpha.1

已完成：

- Rust workspace、Apache-2.0 边界、严格的 `aimedia/v1alpha1` 配置协议。
- 单调时间映射、固定容量 timeline、确定性导播状态机和音频 DSP 原语。
- VLM advisor contract、MPEG-TS packet probe、H.264 Annex-B 和 AAC ADTS parser。
- 流式 TS 同步恢复、PSI/PES 重组、PTS 回绕和 PAT/PMT/PCR mux 往返。
- libsrt 1.5.5 caller/listener 回环、Unix Socket 控制面和 mock 节目调度器。
- NVIDIA Video Codec SDK 13.0 驱动能力探测、RAII surface lease 和 libxaac 符号探测。
- `probe`、`doctor`、`control`、`explain`、`run --dry-run`、`run --mock`、`replay`、`bench`。
- 配置、TS 和 elementary stream 的 Linux CI/fuzz 入口。

仍未完成：

- NVDEC/NVENC 帧提交、GPU surface copy 和 libxaac 帧级编解码。
- 将 SRT、TS、codec、双路 timeline、音频 DSP 和输出调度器串成持续实时数据面。
- SRT 自动重连、网络损伤、真实播放器互操作及 2/24 小时 soak。

## 第二阶段：原生媒体闭环与双机位人工导播

个人开发预计 14—18 周。每个切片独立提交 PR，只有满足完成条件后才在支持矩阵中升级状态。

| 切片 | 主要交付 | 完成条件 |
| --- | --- | --- |
| 2.1 文档与协议 | 新手文档、SRT/控制配置、控制消息 schema | 本 PR 已实现；待合并后标记完成 |
| 2.2 流式 MPEG-TS | 字节同步、PSI/PES 重组、PTS/DTS、TS mux | 本 PR 已通过分块/回绕/mux 往返；真实流与损坏语料仍待扩充 |
| 2.3 SRT | libsrt 1.5.5 caller/listener、重连、统计 | 本 PR 已通过 caller/listener 回环；自动重连和网络损伤仍待实现 |
| 2.4 Codec | NVDEC/NVENC SDK 13.0、libxaac、RAII surface | 1080p30 H.264/AAC 单路硬件转码连续运行 |
| 2.5 单路闭环 | SRT → demux → decode → encode → mux → SRT | 输出被 FFmpeg/VLC 读取；PTS/DTS/PCR 单调 |
| 2.6 双路人工导播 | 同步 timeline、Take/Auto、音频淡化、故障切换 | Take p95 < 100ms；超限机位不可切入；切镜不重连 |
| 2.7 硬化 | GPU 镜像、网络故障、soak、兼容矩阵 | 2 小时日常 soak 和 24 小时发布候选 soak 通过 |

第二阶段明确不包含：

- Silero VAD、人物/人脸、嘴部运动等自动分析器。
- 实时 VLM、HTTP/REST 控制面和动态插件加载。
- 画中画、转场、四机位、RTMP、WHIP/WHEP 或 CPU codec fallback。

## 第三阶段：快脑自动导播

- 接入 VAD、人物/人脸、嘴部运动、画质、冻结和传输健康度。
- 将实时指标转换为 `CameraSnapshot`，复用当前确定性状态机。
- 完成迟滞、冷却、故障切换和可解释的决策事件。
- 在没有 VLM 时独立达到自动切镜验收指标。

## 第四阶段：VLM 慢脑与 SDK

- OpenAI 兼容 VLM adapter、本地模型示例和 mock provider。
- JSON Schema、800ms deadline、3 秒有效期、熔断和隐私控制。
- `FastAnalyzer`、`DirectorAdvisor` 的版本化 C ABI SDK。

## Alpha 与 Beta

- Alpha：网络损伤、漂移、断流、GPU OOM、恶意媒体、SBOM、镜像和兼容矩阵。
- Beta：四机位、自动偏移估算、HTTP/WebSocket 控制面、Prometheus 和生产部署指南。
- 只有经过真实互操作和 soak 的组合才标记为 `supported`；其余保持 `planned` 或 `experimental`。
