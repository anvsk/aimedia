# HEVC 输入桥接验证记录

更新时间：2026-08-10

## 结论

V3-04A/B 的代码边界已经完成：RTSP SDP 可在 H.264 与 H.265 之间选择 NVDEC，HEVC
Main 8-bit 4:2:0 复用现有 NV12 surface lease，再进入单会话 NVENC H.264 输出。SRT/TS
和传统 RTMP 输入仍只接受 H.264，没有借此次变更扩大格式声明。

当前只能把该能力标记为 `foundation`，不能写成已通过 RTSP H.265 闭环。单个 HEVC
Annex-B IRAP 在 RTX 5060 Laptop + 577.12 驱动上已由真实 NVDEC 解码成共享 NV12 surface，
但完整 RTSP 软件源门槛在修复第二个问题后未再次运行。按用户要求，本阶段不继续重复
长门槛或两小时 soak。

## 已通过证据

- Linux x86_64 release 构建通过，使用 Video Codec SDK 13.0，三个固定头文件组合指纹为
  `613e2cd436d4d7fbc283e5d92184e7d7f8739ec680f1ee372d580eb801df9ef2`。
- HEVC Main、1920x1080、8-bit 4:2:0 Annex-B IRAP fixture 由 RTX 5060 Laptop 的 NVDEC
  输出一个 NV12 surface；surface 有非零 device pointer，pitch 不小于显示宽度。
- fixture SHA-256 为
  `1c7fed800f37a683a3a9deb641c9e98fb5e7fc4a4504392654fb977e67875666`。
- 完整 CLI 在固定 SDK Linux builder 中以 release profile 编译成功；运行镜像保持
  `USER 10001`、`ENTRYPOINT ["aimedia"]`，不新增 FFmpeg/libav 运行时依赖。

## 外部 RTSP 门槛发现的问题

第一次 90 秒门槛使用 MediaMTX 1.20.0 和 libx265 软件发布源。RTSP、AAC、SRT 输出、
输入重连和网络损伤流程均工作，但 2,393 个视频 access unit 被恢复闸门丢弃，视频解码
帧数为零。原因是 RTP relay 没有保留库层的 random-access 标记。修复位于 RTSP adapter：
除依赖库标记外，还从 Annex-B NAL header 识别 H.264 IDR type 5 与 HEVC IRAP type 16—23；
运行时仍必须等到安全恢复点，不会接受任意帧。

第二次门槛已越过该闸门，但在启动阶段终止，错误为
`NVDEC parser produced a frame before format`。真实原因并不是已有帧缺少格式，而是 parser
处理参数/预热 access unit 后显示队列仍为空，旧代码却无条件要求 sequence format。
修复后只有在收到真实 display callback 时才要求格式存在。

## 尚未完成

- 修复后的完整 `RTSP/HEVC -> NVDEC -> NVENC H.264 -> SRT/TS` 短门槛尚未重跑。
- 因此还没有本链路的视频 packet、首帧 IDR、PTS/DTS/PCR 单调与断线后 IRAP 恢复证据。
- 物理摄像机、两家不同设备/固件和两小时稳定性属于 V3-02F/V3-04 后续外部门槛。
- H.265 Main10、HDR、隔行、4:2:2/4:4:4、HEVC 输出、Enhanced RTMP 和 SRT/TS HEVC
  都不在本切片范围。

在完整短门槛通过前，支持矩阵必须保持 `foundation`；即使软件源通过，物理设备认证和
长稳完成前，RTSP 整体也只能是 `experimental`。
