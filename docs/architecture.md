# 架构说明

## 数据面

目标数据面为：

```text
2x SRT -> 2x MPEG-TS demux -> 2x NVDEC -> bounded synchronizer
                                            |-> fast analyzers
                                            |-> sampled VLM side channel
                                            `-> deterministic director
director -> selected GPU frame -> 1x NVENC -> MPEG-TS -> SRT
director -> selected PCM -> loudness match/crossfade -> AAC -> MPEG-TS
```

核心 Rust crate 不持有 FFmpeg 类型。外部 transport、codec、GPU 和 inference runtime 通过 `backend` traits 接入；native 插件只暴露版本化 C ABI。

## 时钟和同步

- 两个输入分别建立 source PTS 到 program timeline 的映射。
- `offsetMs` 在映射阶段应用，不回写输入 PTS。
- program encoder 生成唯一、连续、单调的输出 PTS/DTS。
- 同步器只保留固定容量窗口，超过容量淘汰最旧数据。
- 目标机位与 master 的 skew 超过 `maxSkewMs` 时不允许自动切入。
- 时间戳倒退被视为 discontinuity，调用方必须 flush 对应 decoder 和 timeline。

## 双层 AI

快脑处理 VAD、人物、嘴部运动、画质、冻结和传输健康度。它必须在没有 VLM 时独立完成自动导播。

慢脑通过 OpenAI 兼容 JSON Schema 接口获取两个采样画面和快脑指标。结果具有 deadline 和 expiry，权重上限为 25%。超时、限流、无效 JSON 或服务离线只产生事件，不进入媒体故障路径。

## 音频

音频跟随镜头需要两路持续解码为 48kHz 双声道 PCM。切换时先根据 K-weighted 400ms 窗估计增益，再执行 80ms 等功率交叉淡化。当前基础实现提供 sample peak 限制；真正的 4x oversampled true-peak limiter 属于 AAC 后端集成阶段。

## 安全边界

- TS、H.264、AAC 和所有配置均视为不可信输入。
- parser 不得索引未验证长度或创建基于输入声明的无界分配。
- URI 中禁止明文敏感查询参数；使用环境变量或文件型 `secretRef`。
- native codec/GPU FFI 崩溃隔离是 Beta 阶段目标，Alpha 先用 sanitizer、fuzz 和受控进程部署。
