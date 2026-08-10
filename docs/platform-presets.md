# 平台预设契约

平台预设是经过验证的一组默认媒体参数，不包含 endpoint、stream key 或平台 API
凭证。用户可以覆盖非安全参数；任何 secret 仍必须通过环境变量或挂载文件引用。

以下名称仍是计划中的运行时 preset；当前 `tools/platform.ps1` 已先固化相同的媒体参数、
凭证规则和验收条件，用于生成可审计证据，不代表 preset 已经实现或平台已经兼容。

| 预设 | 用途 | 初始媒体约束 | 发布门槛 |
|---|---|---|---|
| `srt-low-latency` | 中外远程贡献与节目回传 | H.264 Main、无 B 帧、1 秒 GOP、AAC-LC 48kHz 128kbps、MPEG-TS | OBS、VLC、FFmpeg 测试端和网络损伤 |
| `cn-rtmp-standard` | 中国直播云与通用 RTMP 平台 | H.264、CBR、2 秒 GOP、AAC-LC 48kHz、FLV/RTMP | 腾讯云、阿里云真实 endpoint |
| `youtube-rtmps` | YouTube Live | H.264、CBR、2 秒 GOP、AAC-LC、RTMPS | 非公开直播和平台 health 无 codec 错误 |
| `twitch-rtmp` | Twitch | H.264、CBR、2 秒 GOP、AAC-LC、RTMP/RTMPS | Twitch bandwidth test |
| `whip-low-latency` | 标准 WebRTC 发布 | H.264/Opus、无 B 帧、Bearer token、WHIP | OBS 及两个独立 WHIP 服务 |

## 配置规则

- 预设只选择参数默认值，不能绕过 `aimedia/v1alpha2` 的格式和范围校验；
- 明确写在配置中的参数优先于预设；
- endpoint scheme 必须与预设兼容；
- stream key 使用 `streamNameRef`；平台把签名放在 publish URL query 时，使用独立的
  `publishQueryRef`。二者都只能引用环境变量或挂载文件，不能拼进 URI；
- 预设的状态独立于 transport 状态；未完成真实 endpoint 验证时不得标记
  `supported`；
- 平台修改官方要求后，先更新兼容测试，再更新预设默认值。

## 不提供的能力

- 自动登录平台、创建直播间或管理评论；
- 在配置文件内保存 stream key；
- 绕过平台地区、账号、内容或合规限制；
- 将没有验证过的社交平台归类为兼容。

## 当前前置门槛

`aimedia publish-check -f <job.yaml> --json` 只建立发布会话，不打开输入、codec 或 GPU。
它适合区分连接、TLS/RTMP 握手和 publish 拒绝，但不能证明媒体编码参数被平台接受。
`tools/platform.ps1` 在此基础上增加 YouTube、Twitch、腾讯云和阿里云的凭证形状检查；
默认完整模式还要求 GPU 媒体包持续发送。结果见[平台发布门槛报告](reports/platforms.md)。
