# RFC 0003：RTMP/RTMPS 与 FLV 边界

- 状态：Accepted for staged implementation
- 日期：2026-08-06
- 路线图：V3-03

## 为什么现在做

SRT 适合远程贡献，RTSP 适合摄像机接入，但国内外大多数直播平台仍把 RTMP 或
RTMPS 作为共同发布入口。V3-03 的目标不是建设 RTMP 媒体服务器，而是补上两个真实
工作流：

1. OBS、硬件编码器或上游服务向 aimedia 的单连接 RTMP listener 发布 H.264/AAC。
2. aimedia 把处理后的 H.264/AAC 通过 RTMP 或 RTMPS 推送到直播云和创作者平台。

RTMP pull、观众播放、GOP cache、边缘分发、HTTP-FLV、Enhanced RTMP、HEVC 和 AV1
不进入本阶段。

## 协议在流水线里的位置

```mermaid
flowchart LR
    P["OBS / 编码器"] -->|"RTMP chunks"| RI["RTMP listener"]
    RI --> FD["FLV tag 解析"]
    FD -->|"H.264 Annex-B"| VD["NVDEC"]
    FD -->|"AAC ADTS"| AD["AAC decode"]
    VD --> TL["独立节目时间线"]
    AD --> TL
    TL --> VE["NVENC H.264"]
    TL --> AE["AAC encode"]
    VE --> FM["FLV AVC tag 生成"]
    AE --> FM
    FM --> RO["RTMP / RTMPS publisher"]
    RO --> C["直播平台或直播云"]
```

RTMP 是连接、握手、命令和分块传输协议；FLV tag 是这条连接中承载音视频的媒体格式。
两者必须分层，不能把网络重连逻辑混进 H.264/AAC 转换。

## P0 媒体约束

- 输入：RTMP listener，单端点同时只接受一个 publisher。
- 输出：RTMP 或 RTMPS publisher；RTMPS 使用 rustls 和系统信任根，禁止关闭证书校验。
- 视频：传统 FLV AVC tag 中的 H.264 8-bit 4:2:0；不接受 Enhanced RTMP codec。
- 音频：AAC-LC；不接受 MP3、Opus 或厂商私有音频 tag。
- 分辨率、帧率和采样率仍受当前 MediaJob profile 限制。
- 每条连接的单个 RTMP message 默认最多 8 MiB，配置硬上限为 16 MiB。

### H.264 转换

RTMP/FLV 使用 `AVCDecoderConfigurationRecord` 保存 SPS/PPS，并用长度前缀保存 NAL unit；
当前 NVDEC/NVENC 边界使用 Annex-B start code。输入端必须先收到 AVC sequence header，
把长度前缀 NAL 转为 Annex-B，并等到 IDR 后才开放解码。输出端从 NVENC 的 SPS/PPS
构建 sequence header，把 access unit 转为长度前缀形式。

重连后不得续用旧连接的配置状态。输出必须重新发送 metadata、AVC sequence header、
AAC sequence header，并请求新的 IDR。

### AAC 转换

FLV 的 AAC sequence header 保存 `AudioSpecificConfig`，普通音频 tag 只保存 AAC raw
payload；当前 AAC decoder 接收 ADTS。输入端据此为 raw payload 重建 ADTS，输出端从
ADTS 提取配置和 raw payload。没有合法 sequence header 时不把音频送入 decoder。

## 时钟和重连

- RTMP timestamp 是每连接 32-bit 毫秒值；输入适配器负责回绕展开，只用于映射。
- 输出 timestamp 来自 aimedia 独立节目时钟，不能直接延续输入 RTMP timestamp。
- 输入 publisher 断开后清空握手、FLV 配置和时间映射；新 publisher 从 sequence header
  与 IDR 重新开始。
- 输出断开时丢弃历史编码包，不在内存中等待平台恢复；指数退避重连后重新发配置和 IDR。
- 网络读写、decoded event、codec 和发送队列都有硬上限；没有“临时”无界队列。

## 配置契约

URI 只保存主机和 application path，stream name 单独配置。这样错误和日志可以显示
endpoint 阶段而不泄露平台 stream key。

```yaml
inputs:
  - name: encoder
    uri: rtmp://0.0.0.0:1935/live
    rtmp:
      mode: listen
      streamName: camera

outputs:
  - name: program
    uri: rtmps://publish.example.test/live
    rtmp:
      mode: publish
      streamNameRef:
        env: AIMEDIA_RTMP_STREAM_NAME
```

规则：

- 输入只接受 `rtmp://` + `mode: listen`；Alpha 不提供 RTMPS listener。
- 输出接受 `rtmp://` 或 `rtmps://` + `mode: publish`。
- `streamName` 与 `streamNameRef` 必须二选一；任何凭证都必须使用引用。
- URI userinfo、query、fragment 和尾随 `/` 被拒绝，避免 stream key 混入错误或日志。
- `connectTimeoutMs` 管 TCP，`handshakeTimeoutMs` 管 TLS/RTMP 建连，`readTimeoutMs`
  管建连后的对端失活。
- `maxMessageBytes` 默认 8 MiB，可配置范围 64 KiB—16 MiB。

当前 `aimedia explain` 和 `aimedia run --dry-run` 会展示完整 pending 图；真实运行会在
打开网络、codec 或 GPU 前返回 `rtmpDataPlanePending`。

## Rust 依赖决定

协议核心首选且等待 V3-03B 门禁的候选版本是
[`shiguredo_rtmp 2026.1.0-canary.6`](https://crates.io/crates/shiguredo_rtmp/2026.1.0-canary.6)：

- Apache-2.0、Rust 1.88、零第三方依赖、`no_std`、Sans-I/O。
- 同时提供 publish client、play client 和 server connection 状态机。
- socket、超时、rustls、任务和背压由 aimedia 持有，不把 Tokio runtime 藏进协议库。
- 在 aimedia 的 Rust 1.88 工具链上，上游 89 项 library tests 全部通过。

它仍是 canary，不能未经隔离直接泄漏到公共 API。V3-03B 必须先在短目录
`crates/rtmp` 中完成回环、最大 message、恶意 chunk stream、发送缓冲和断线测试，再把
依赖加入 workspace；所有上游类型都留在 crate 内部。若这些门禁失败，回退到自有小型
状态机或重新评估 [`rtmp-rs`](https://github.com/torresjeff/rtmp-rs)，不让应用层绑定某个
候选库。

未选择的主要候选：

| 候选 | 结果 | 原因 |
|---|---|---|
| `rtmp-rs 0.5.0` | fallback | client/server 与 rustls 齐全，但高层 Tokio server/registry 较重，边界和队列不完全由 aimedia 控制 |
| `oxideav-rtmp 0.0.6` | 暂不选 | ingest/push 功能多，但采用阻塞式连接模型，RTMPS 尚不是一等能力 |
| `scuffle-rtmp 0.2.3` | 不选 | 适合 server ingest，但没有 P0 所需的 publisher client |
| `librtmp2 0.5.0` | 不选 | Rust 1.93 高于项目 MSRV，默认 TLS 依赖 OpenSSL |

依赖选择依据是边界和可控性，不是功能数量或 GitHub star。

## 交付拆分

- V3-03A：本 RFC、配置 schema、pending 执行图、稳定错误码和示例。
- V3-03B：`crates/rtmp` Sans-I/O 会话适配器、上限保护、RTMP 明文回环。
- V3-03C：AVC/AAC sequence header、Annex-B/AVCC、ADTS/raw 双向转换。
- V3-03D：RTMP listener 输入接入单路原生运行时及重发布恢复。
- V3-03E：RTMP/RTMPS publisher、rustls、重连、配置重发和 IDR 闸门。
- V3-03F：OBS/FFmpeg/MediaMTX 互操作、网络故障、平台前置 smoke 和两小时 soak。

只有 V3-03F 通过后，支持矩阵才把 RTMP/RTMPS + FLV 升级为 `supported`。
