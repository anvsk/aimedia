# 市场支持策略

## 产品边界

`aimedia` 是直播制作与发布引擎，不是观众侧 CDN 或播放器。协议进入核心的条件是：

- 能覆盖至少两个目标平台，或一个明确的区域用户故事；
- 有公开规范或可审计的依赖边界；
- 有测试端、真实 endpoint 或可重复的兼容语料；
- 不会让媒体主链依赖模型、外部控制面或无界缓冲。

## 优先级矩阵

| 优先级 | 市场 | 输入 | 输出 | Codec 和封装 | 目标里程碑 |
|---|---|---|---|---|---|
| P0 | 中外共同 | SRT/MPEG-TS、RTMP/RTMPS | SRT/MPEG-TS、RTMP/RTMPS | H.264/AAC；TS/FLV | v0.2—v0.4 |
| P1 | 中国大陆 | RTSP/RTP、SRT | RTMP/RTMPS | H.264/H.265 输入；AAC/G.711；H.264/AAC 输出 | v0.6 |
| P1 | 海外 | SRT/RTMP | RTMPS、WHIP | H.264/AAC；WHIP H.264/Opus | v0.4、v0.7 |
| P2 | 平台增强 | HLS ingest、Enhanced RTMP | 多编码输出 | HEVC/AV1、CMAF/HLS | 采用数据驱动 |
| 非核心 | 监控、传统广播、CDN | GB28181、RIST、NDI、SDI | HTTP-FLV、HLS/LL-HLS | AVS3、VP9、字幕、DRM | 插件或长期 |

## 中国大陆方向

共同发布基线采用 RTMP/RTMPS H.264/AAC；远程贡献采用 SRT/MPEG-TS。摄像机接入
在共同内核稳定后增加 RTSP/RTP、H.265 decode 和 G.711 decode。

阿里云 ARTC/RTS、腾讯云厂商 SDK 等专有能力不进入核心。官方示例优先使用其公开
RTMP、RTMPS 或 SRT endpoint。抖音、快手和 Bilibili 必须在真实账号验证后才从
`experimental` 升级。

参考：

- [腾讯云直播协议](https://intl.cloud.tencent.com/en/document/product/267/7968)
- [腾讯云 SRT Push](https://intl.cloud.tencent.com/document/product/267/40102)
- [阿里云直播限制](https://help.aliyun.com/zh/live/product-overview/limits)
- [阿里云直播封装](https://help.aliyun.com/zh/live/user-guide/live-package-development-guide/)

## 海外方向

YouTube、Twitch 和通用平台发布先使用 RTMPS H.264/AAC。远程制作继续使用 SRT。
WHIP/Opus 在 RTMPS 兼容矩阵稳定后增加，用于低于一秒的发布场景。

HEVC/AV1、YouTube HLS ingest 和 Enhanced RTMP 目前属于增强能力，不阻塞共同
市场基线。

参考：

- [YouTube live encoder settings](https://support.google.com/youtube/answer/2853702)
- [YouTube HLS ingest](https://support.google.com/youtube/answer/10349430)
- [Twitch video broadcast](https://dev.twitch.tv/docs/video-broadcast/)
- [Twitch broadcasting guidelines](https://help.twitch.tv/s/article/broadcasting-guidelines)
- [OBS SRT](https://obsproject.com/kb/srt-protocol-streaming-guide)
- [OBS WHIP](https://obsproject.com/kb/whip-streaming-guide)

## 状态证据

支持状态只描述已经验证的组合：

- `supported`：真实媒体连续运行，并完成外部互操作；
- `foundation`：协议或 backend 基础存在，但用户闭环未完成；
- `experimental`：真实实现存在，但平台、设备或稳定性覆盖不足；
- `planned`：只有市场选择与验收门槛，没有实现；
- `out-of-scope`：当前核心明确不处理。

每次状态升级必须在 `support-matrix.md` 中记录 producer、consumer、平台、GPU、
驱动、时长和已知限制。
