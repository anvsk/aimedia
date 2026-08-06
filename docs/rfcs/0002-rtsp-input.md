# RFC 0002：主流摄像机 RTSP/RTP 输入

- 状态：Accepted
- 日期：2026-08-06
- 影响版本：v0.3

## 用户闭环

中国大陆和海外的直播集成商都需要把网络摄像机接入同一套媒体作业。本阶段只解决
“拉流输入”，不实现 RTSP server、录像回放、ONVIF 设备管理、云台控制或观众播放：

```text
RTSP 摄像机 -> RTP 音视频 -> access unit / PCM -> 现有节目时间线
            -> NVDEC / audio decode -> NVENC + AAC -> SRT 节目输出
```

H.264 + AAC/G.711 会在 V3-02 形成真实单路闭环。H.265 在本阶段完成 SDP 识别、RTP
重组和类型化输出，但 NVDEC HEVC 到 NVENC H.264 的闭环属于 V3-04；在此之前必须报告
`videoBridgePending`，不能把“能收到 RTP 包”写成“支持 H.265 摄像机”。

## 为什么复用 retina

新增短目录 `crates/rtsp`，公开 Cargo 包名为 `aimedia-rtsp`。边界内部固定并审查 Rust
原生 `retina 0.4.19`，不把其类型暴露给 `core`、`graph` 或 `runtime`。

选择复用而不是独立重写 RTSP 的原因：

- RTSP 状态机本身不是 aimedia 的差异化价值，廉价摄像机的非标准行为和鉴权兼容才是
  主要成本；
- retina 已用于 ONVIF RTSP/1.0 摄像机场景，具备 Basic/Digest、TCP interleaved、
  UDP RTP、RTCP，以及 H.264、H.265、AAC 和 G.711 depacketizer；
- 其许可证为 MIT/Apache-2.0，V3-02B 已将 workspace MSRV 和 Docker builder
  统一为 Rust 1.88，不会隐式依赖更新编译器；运行时
  不引入 FFmpeg、GStreamer 或 `libav*`；
- 独立 adapter 可固定我们自己的事件、错误和重连契约，未来更换实现不改变 MediaJob。

代价是新增一组 Rust 依赖，并承担上游 API/行为变化。版本先精确锁定，依赖通过
`cargo deny`、源码许可证和最小 feature 审查；升级必须重新跑摄像机兼容矩阵。

参考规范与实现：

- [RTSP 1.0 / RFC 2326](https://www.rfc-editor.org/info/rfc2326/)
- [RTP / RFC 3550](https://www.rfc-editor.org/info/rfc3550/)
- [SDP / RFC 8866](https://www.rfc-editor.org/info/rfc8866/)
- [H.264 RTP / RFC 6184](https://www.rfc-editor.org/info/rfc6184/)
- [H.265 RTP / RFC 7798](https://www.rfc-editor.org/info/rfc7798/)
- [AAC RTP / RFC 3640](https://www.rfc-editor.org/info/rfc3640/)
- [G.711 RTP profile / RFC 3551](https://www.rfc-editor.org/info/rfc3551/)
- [HTTP Digest / RFC 7616](https://www.rfc-editor.org/info/rfc7616/)
- [ONVIF Profile T](https://www.onvif.org/profiles/profile-t/)
- [retina 0.4.19](https://docs.rs/retina/0.4.19/retina/)

## 固定支持范围

### RTSP 会话

- 只做 RTSP client，支持 `OPTIONS`、`DESCRIBE`、每个媒体流的 `SETUP`、聚合 `PLAY`、
  keepalive 和尽力 `TEARDOWN`。
- 只接受 `rtsp://`。RTSP over TLS、HTTP tunnel、multicast、RTSP 2.0 和 SRTP 暂不支持。
- Basic 与 Digest 鉴权；用户名可写配置，密码只能使用环境变量或挂载文件引用。
- 默认 RTP over RTSP/TCP interleaved；当前 native runtime 明确拒绝 UDP。UDP unicast
  只有在有界重排实现、真实摄像机和网络损伤测试完成后才能升级为
  `experimental`。
- 每个端点只选择一个视频流和最多一个音频流；多 profile/多码流选择在 SDP 阶段按
  MediaJob 约束完成，不在运行中猜测。

### SDP 与媒体

- H.264：`packetization-mode=0/1`，single NAL、STAP-A、FU-A，输出 Annex-B access unit；
  模式 2、STAP-B、MTAP 和 FU-B 明确拒绝。
- H.265：single NAL、AP、FU，输出 Annex-B access unit；真正转 H.264 留给 V3-04。
- AAC：只接受 `MPEG4-GENERIC`、AAC-LC、44.1/48kHz、单/双声道；根据 SDP
  `AudioSpecificConfig` 和 AU header 恢复 AAC 单元，进入统一音频解码/重采样阶段。
- G.711：静态或显式 `PCMA`/`PCMU`，8kHz 单声道，直接解码为交错 `f32 PCM`。
- 摄像机元数据、JPEG、G.726、MP2、AMR、LATM、B 帧重排和厂商私有 payload 不在本阶段。

### 时间和有界性

- RTP sequence 使用固定重排窗口；超窗、重复或永久缺包只丢弃受影响 access unit。
- 32-bit RTP timestamp 按流回绕展开，RTCP Sender Report 只用于源时间映射；输出仍由
  aimedia 独立节目时钟生成。
- 多轨 `PLAY` 的 `RTP-Info` 缺少任一 `rtptime` 时使用 Retina permissive 策略，把各轨
  第一包映射到 NPT 0；这兼容 MediaMTX 等常见服务端，又不改变输出独立节目时钟。
  后续 32-bit 时间戳仍受最大 10 秒前跳和非倒退检查约束。
- 每个流的 RTP 重排、access unit 重组和 adapter 输出队列都有硬上限；NAL/AU 超过
  上限时丢弃当前单元并等待下一个恢复边界。
- H.264/H.265 在重新连接、SSRC 改变、sequence 跳变或参数集变化后等待下一张可独立
  解码的画面；绝不把新旧会话字节拼成一个 access unit。

### V3-02D 实现证据与 UDP 决策

TCP 会话恢复由 `crates/rtsp` 内部完成：读超时、对端断流或可重试的会话错误
会把连接状态置为 false，按 MediaJob 配置做有上限的指数退避。新会话必须重新完成
`DESCRIBE/SETUP/PLAY` 并与初始音视频规格兼容；否则返回稳定的
`mediaProfileChanged` 错误。重连期间不建立媒体队列，节目时钟继续走最后健康画面和
静音路径。已经建立的输入重连时，`DESCRIBE 404` 表示发布路径暂时离线并继续退避；
首次启动仍立即返回错误，401/403 和其他不可恢复 4xx 也不会无限重试。

固定的 Retina 0.4.19 尚无 UDP 重排缓冲，并且在它生成公开
`PacketItem` 之前就会丢弃乱序包，因此 aimedia 无法在 adapter 外层恢复这些包。截至
2026-08-06，该行为在 Retina main 中也仍然存在。证据见其
[`Transport::Udp` 文档](https://github.com/scottlamb/retina/blob/v0.4.19/src/client/mod.rs#L1052-L1067)
和
[`RtpHandler` 乱序分支](https://github.com/scottlamb/retina/blob/v0.4.19/src/client/rtp.rs#L77-L83)；
[当前 main 文档](https://github.com/scottlamb/retina/blob/main/src/client/mod.rs#L1052-L1067)
作为实时复核入口。

上游 [issue #40](https://github.com/scottlamb/retina/issues/40) 仍未完成。早期
[原型 PR #107](https://github.com/scottlamb/retina/pull/107) 的实测结论是：现有 jitter
buffer 对视频的假设不成立，且没有重传机制时常会引入延迟而收益很小；未合并的
[PR #111](https://github.com/scottlamb/retina/pull/111) 还会在一次输入释放多个缓存结果时
遗失部分结果，不可直接依赖。

因此 v0.3 把 UDP 从发布阻塞项改为需求触发的延后项，并继续在 native runtime 明确拒绝，
不降级成“局域网上可能能用”的虚假支持。若真实摄像机或客户数据证明 UDP 为必须，恢复时的
完成条件仍是：sequence/loss 判定和 depacketize 之前具备按流隔离、固定容量和固定等待时间的
重排窗口，通过网络损伤实验后才能升为 `experimental`。

## MediaJob 契约

RTSP 输入沿用 `inputs[].uri`，新增协议专属配置：

```yaml
inputs:
  - name: camera
    uri: rtsp://192.0.2.10/Streaming/Channels/101
    rtsp:
      transport: tcp
      username: admin
      passwordRef:
        env: AIMEDIA_CAMERA_PASSWORD
      connectTimeoutMs: 3000
      readTimeoutMs: 5000
      keepaliveMs: 15000
      reconnect:
        enabled: true
        initialBackoffMs: 250
        maxBackoffMs: 5000
```

URI 中的 userinfo、`password`、`token` 和厂商密钥 query 一律拒绝并从日志隐藏。SRT 与
RTSP 配置不能同时生效；scheme 与协议专属字段冲突时在打开网络之前失败。

## 数据流与故障边界

```mermaid
flowchart LR
    CFG["MediaJob RTSP input"] --> SESSION["RTSP session + auth"]
    SESSION --> SDP["SDP stream selection"]
    SDP --> RTP["bounded RTP/RTCP receiver"]
    RTP --> VDEPAY["H.264/H.265 depacketizer"]
    RTP --> ADEPAY["AAC/G.711 depacketizer"]
    VDEPAY --> VIDEO["typed video access unit"]
    ADEPAY --> AUDIO["AAC unit or f32 PCM"]
    VIDEO --> EXISTING["existing decode + program timeline"]
    AUDIO --> EXISTING
    EXISTING --> OUTPUT["existing bounded SRT output"]
```

DNS、TCP connect、401/403、DESCRIBE/SDP、SETUP transport、PLAY、RTP timeout、codec 和
payload 损坏必须使用稳定阶段码。可恢复的网络/会话错误只重建该输入；格式不支持在启动
前终止；队列满按契约丢旧数据或背压，禁止无界缓存。

## 验收门槛

- CPU：RTSP message/SDP fixture、RTP 回绕/乱序/重复/缺包、H.264/H.265 分片、AAC AU、
  G.711 解码、大小上限和错误码。
- 互操作：FFmpeg 测试 server、MediaMTX，以及至少两台不同厂商或两个 ONVIF 合规设备；
  每项记录型号/固件/transport/codec，模拟器不能替代真实设备证据。
- 网络：TCP、1% 丢包、20ms 抖动、40ms RTT、会话超时、401 拒绝和输入重连；UDP
  按 V3-02D 的产品决策延后，不属于 v0.3 发布门槛。
- GPU 闭环：H.264 + AAC/G.711 输入，1080p30 输出 SRT，PTS/DTS/PCR 单调；H.265 只验
  depacketizer，直到 V3-04 才升级完整支持。
- 稳定性：两小时运行中所有队列和重组缓存有水位，RSS/GPU 内存不持续增长；凭证不在
  日志、状态、错误或崩溃上下文出现。

## PR 切片

1. 契约、依赖审查、MediaJob schema 和 fixture 语料。
2. `crates/rtsp` adapter：会话、鉴权、SDP 与类型化媒体事件。
3. TCP interleaved H.264/AAC/G.711 单路运行时闭环。
4. TCP 超时/断流重连、规格一致性和独立 discontinuity。
5. UDP RTP/RTCP、有界重排和网络损伤恢复。
6. H.265 depacketizer 边界与 V3-04 handoff。
7. 外部设备、网络损伤、两小时 soak 和支持矩阵升级。
