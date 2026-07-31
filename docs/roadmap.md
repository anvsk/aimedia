# 实施路线图

## 已完成的基础切片

- Workspace、Apache-2.0 边界、严格配置协议。
- 时间戳、固定容量同步、导播和音频 DSP 原语。
- VLM adapter contract、clean-room MPEG-TS probe、H.264 Annex-B 和 AAC ADTS parser。
- replay/benchmark harness，便于在 native 后端前验证导演行为。
- 配置、TS 和 elementary stream 的 Linux fuzz/CI 入口。

## 下一里程碑：单路媒体闭环

1. 实现动态链接 `libsrt` 的 transport adapter。
2. 增加 PSI section 与 PES reassembly。
3. 在现有 H.264 Annex-B NAL 与 AAC ADTS frame parser 上增加 access-unit/PES reassembly。
4. 打通单路 SRT capture/probe/relay。
5. 在 Linux NVIDIA CI runner 上接入 NVDEC/NVENC。
6. 接入 libxaac 并用 program clock 重新编码单路节目。

## 双路导播里程碑

1. 把 decoder output 写入两个固定容量 timeline。
2. 以 master program time 选择最近帧和 PCM block。
3. 接通 manual take、音频交叉淡化和 IDR 请求。
4. 接通 Silero VAD、人物/人脸、嘴部运动和画质分析器。
5. 将 native telemetry 转换为 `CameraSnapshot`，复用当前状态机。

## Alpha 硬化

- 网络损伤、输入漂移、断流、GPU OOM、VLM 离线和恶意媒体测试。
- 24 小时 soak、Prometheus、SBOM、容器镜像和兼容矩阵。
- 只有两路 1080p30 H.264/AAC SRT 链路通过后，才标记为 supported。
