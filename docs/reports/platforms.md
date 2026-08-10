# 平台发布门槛报告

## 结论

2026-08-10 完成了 V3-03F3a/F3b：aimedia 现在能安全表达直播平台常见的 stream key
与签名 query，能在不启动输入、codec 或 GPU 的情况下预检 RTMP/RTMPS publish，并且
成功或失败都会生成不含凭证的阶段报告。固定的 MediaMTX 1.20.0 已验证 accepted 路径。

V3-03F3 仍未完成。YouTube 和 Twitch 的公开 ingest endpoint 在当前机器与 Docker 中都
解析到 `198.18.0.0/15` 保留地址，TCP 可连，但 RTMP 握手被代理路径关闭；同一容器的
FFmpeg 也失败。因此本报告证明门槛和诊断有效，不证明 YouTube、Twitch、腾讯云或
阿里云已经兼容。

## 为什么需要两种秘密引用

平台推流地址并不统一：

- YouTube 通常把 stream key 放在 RTMPS URL 的最后一段；官方要求从 Live Control
  Room 复制 RTMPS URL 和 stream key；
- Twitch 的 bandwidth test 在 stream key 后增加 `bandwidthtest=true`；
- 腾讯云签名推流地址使用 `txSecret` 和 `txTime`；
- 阿里云 URL 鉴权使用 `auth_key`。

参考：[YouTube RTMPS](https://support.google.com/youtube/answer/10364924)、
[Twitch broadcast](https://dev.twitch.tv/docs/video-broadcast/)、
[腾讯云推流 URL](https://intl.cloud.tencent.com/document/product/267/31558)、
[阿里云 URL 鉴权](https://help.aliyun.com/zh/live/user-guide/url-signing)。

此前配置拒绝所有 RTMP URI query，导致后三类地址只能把签名拼进 URI，既无法通过校验，
也容易被错误和日志泄漏。现在契约固定为：

```yaml
uri: rtmp://publish.example.test/live
rtmp:
  mode: publish
  streamNameRef:
    env: AIMEDIA_RTMP_STREAM_NAME
  publishQueryRef:
    env: AIMEDIA_RTMP_PUBLISH_QUERY
```

`publishQueryRef` 的值不包含开头 `?`，最多 512 bytes，只接受无空格、换行、fragment
或嵌套 query 的可打印 ASCII。URI、Debug、错误和 JSON 报告只展示 base endpoint。

## 门槛工具

### 无媒体预检

```bash
aimedia publish-check -f job.yaml --json
```

该命令读取真实输出配置并完成 TCP、TLS、RTMP handshake 和 publish command；成功后
立即关闭。它不打开输入、codec 或 GPU，所以适合快速区分 endpoint/证书/鉴权问题。
publish 被平台拒绝时返回稳定的 `PublishRejected during Command`，不会输出服务端原始
原因或 stream key。

### 平台门槛

```powershell
$env:AIMEDIA_PLATFORM_STREAM_NAME = '<temporary-stream-key>'
$env:AIMEDIA_PLATFORM_QUERY = 'bandwidthtest=true'
pwsh ./tools/platform.ps1 `
  -Platform twitch `
  -Endpoint rtmp://<official-ingest>/app `
  -PublishQueryEnv AIMEDIA_PLATFORM_QUERY `
  -ExpectPublishReject
```

脚本只接受不含 userinfo、stream name、query 和 fragment 的 base endpoint。YouTube 必须
使用 RTMPS；Twitch、腾讯云、阿里云分别检查所需 query 字段。`-HandshakeOnly` 和
`-ExpectPublishReject` 不要求 GPU；默认模式则启动 SRT 测试源与 GPU 数据面，要求平台
连接持续存在、发送包递增、重连为零且日志不含凭证。

## 本轮结果

| 目标 | 期望 | 观察 | 结论 |
|---|---|---|---|
| MediaMTX 1.20.0，`rtmp://host.docker.internal:1937/live` | accepted | accepted，engine exit 0 | 门槛成功路径通过；不是公共平台证据 |
| YouTube，`rtmps://a.rtmps.youtube.com/live2` | 无效临时 key 应进入鉴权拒绝 | `Io during Handshake` | 未到鉴权，不能计入平台通过 |
| Twitch Singapore，`rtmp://aps10.contribute.live-video.net/app` | 无效临时 key + bandwidth test 应被拒绝 | `Io during Handshake` | 未到鉴权，不能计入平台通过 |

本轮使用的是随机生成的无效 key，没有使用、打印或保存真实 stream key。YouTube DNS
映射为 `198.18.0.116`，Twitch 映射为 `198.18.0.117`；这两个地址属于当前网络代理的
合成路径。YouTube 的 TLS 证书校验先完成，随后 RTMP handshake read 失败；Twitch 在
明文 RTMP handshake 被关闭。FFmpeg 8.1.2 对两者也返回 I/O error，说明当前失败不能
归因于 aimedia 的 RTMP 命令实现。

脱敏本机报告：

- MediaMTX accepted：`aimedia-platform-05d21291/summary.json`，SHA-256
  `90f8a444a3a5e9ca80c9f5d5f8ef091f55e1a6120d227c7b5290f429025b3ef3`；
- YouTube：`aimedia-platform-68ad073f/summary.json`，SHA-256
  `7c7c3cf70a66688ab6c9839e2a3701164ac3b2d9d036b58e42f8e8ddaf263135`；
- Twitch：`aimedia-platform-694a6705/summary.json`，SHA-256
  `3c7619b1bd9c4598a120669c200c193936744ac7eda1ce7d19f2fcb1ade1ed4c`。

这些临时报告不是 Release 资产。V3-03F3c 完成时必须保存至少两个平台的 accepted
握手、30 秒以上媒体状态和平台控制台健康证据，并确保至少一个平台使用 RTMPS；届时
再把原始脱敏报告作为 Release 附件保存。

## 尚未完成

- 至少两个真实平台使用临时授权或测试频道返回 accepted；
- 每个平台持续接收 H.264/AAC 媒体不少于 30 秒，控制台无 codec/GOP 错误；
- 至少一个 RTMPS 平台完成真实证书、publish 授权和媒体接收；
- 完成前 RTMP/RTMPS 继续标记 `experimental`，不发布“平台兼容完成”的社区广告。
