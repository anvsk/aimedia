# Docker 镜像

默认根目录 `Dockerfile` 是 CPU/core 镜像，用于配置、TS、回放和 mock control 验证：

```bash
docker build -t aimedia:core .
docker run --rm aimedia:core doctor --json
```

GPU probe 镜像构建 libsrt 1.5.5 和固定提交的 Android libxaac，并在运行时使用
宿主机由 NVIDIA Container Toolkit 注入的驱动库。该目标不生成 proprietary SDK
bindings：

```bash
docker build -f docker/Dockerfile.gpu --target probe-runtime -t aimedia:gpu-probe .
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  aimedia:gpu-probe doctor --strict
```

`doctor --strict` 会验证 libsrt、CUDA/NVDEC/NVENC driver libraries，并实际创建后
释放 libxaac 编码/解码上下文。它只检查环境，不运行视频帧；完整单路直播数据面仍以
[路线图](../docs/roadmap.md)的完成标记为准。

`NVIDIA_DRIVER_CAPABILITIES` 必须包含 `video`。只传 `compute,utility` 时，NVIDIA
Container Toolkit 不会把 `libnvcuvid.so.1` 和 `libnvidia-encode.so.1` 注入容器，
NVDEC/NVENC 会被判断为不可用。

## 默认 GPU 构建：固定 ABI headers

参考 GPU 镜像默认从
[`nv-codec-headers` n13.0.19.0](https://github.com/FFmpeg/nv-codec-headers/tree/n13.0.19.0)
的固定 commit `e844e5b26f46bb77479f063029595293aa8f812d` 生成 Video Codec SDK
13.0 ABI bindings：

```bash
docker build -f docker/Dockerfile.gpu \
  --target sdk-build-test \
  -t aimedia:sdk-test .

docker build -f docker/Dockerfile.gpu \
  --target sdk-runtime \
  -t aimedia:gpu .
```

这些是 MIT 许可的构建期 headers，不会引入 `ffmpeg` 可执行文件或 `libav*` 运行时，
也不会进入最终镜像；最终镜像只保留其许可证。构建脚本还会核对 README 中对应的
SDK 版本并计算四个 ABI headers 的组合 SHA-256。

## 可选 GPU 构建：NVIDIA 官方 SDK named context

Video Codec SDK 13.0 的 proprietary archive 和 headers 不提交到仓库。用户必须从
NVIDIA 官方渠道接受许可证、下载并解压 SDK。context 根目录必须直接包含
`Interface/nvEncodeAPI.h`、`Interface/nvcuvid.h` 和 `Interface/cuviddec.h`。

PowerShell 示例：

```powershell
docker buildx build `
  -f docker/Dockerfile.gpu `
  --build-context video_codec_sdk='C:\sdk\Video_Codec_SDK_13.0' `
  --target sdk-runtime-official `
  -t aimedia:gpu-sdk .
```

构建脚本检查 `NVENCAPI_MAJOR_VERSION=13` 和 `NVENCAPI_MINOR_VERSION=0`，对三个
headers 的文件名与内容计算组合 SHA-256，并把版本和 fingerprint 编译进
`doctor --json` 报告。首次构建会在日志中显示 fingerprint；受控发行可在后续构建
中锁定它：

```powershell
docker buildx build `
  -f docker/Dockerfile.gpu `
  --build-context video_codec_sdk='C:\sdk\Video_Codec_SDK_13.0' `
  --build-arg VIDEO_CODEC_SDK_EXPECTED_SHA256='<64-hex-fingerprint>' `
  --target sdk-build-test-official .
```

SDK headers 和 CUDA headers 只存在于 builder stage，不进入运行镜像。未传 named
context 时，两个 `*-official` 目标必须明确失败；默认 `sdk-runtime`、普通 CPU CI 和
`probe-runtime` 都不需要 NVIDIA 开发者账号。

GPU 运行镜像将 libsrt 与 libxaac 的许可证和 NOTICE 安装到
`/usr/local/share/licenses/`；SDK headers 不进入最终镜像。

`native-test` 目标还会真实验证 SRT caller/listener 回环和断线恢复：listener 在首个
caller 断开后按配置重建 socket，重连期间统计保持可读，第二个 caller 的消息到达后
`reconnects` 才递增。发送端 adapter 不维护历史消息队列，退避期间的包由上层按有界
队列策略丢弃。

## 外部互操作工具

`Dockerfile.test` 只用于验收；`network` 目标包含 FFmpeg 和 `tc netem`。`desktop`
目标从固定 Ubuntu 基础镜像单独构建，包含 VLC、VLC SRT access plugin、OBS 和
Xvfb，不继承 FFmpeg 工具镜像中的 `/usr/local` 媒体库。这些工具不会进入 aimedia
运行镜像。PowerShell 脚本覆盖输入/输出的四种 SRT caller/listener 组合，并检查
codec、首帧 IDR 和 PTS/DTS 单调性：

```powershell
docker build -f docker/Dockerfile.test --target network -t aimedia:test-tools .
pwsh ./tools/interop.ps1 `
  -EngineImage aimedia:gpu `
  -PeerImage aimedia:test-tools `
  -SkipToolBuild
```

OBS/VLC 互操作验收使用完整工具目标：

```powershell
docker build -f docker/Dockerfile.test --target desktop -t aimedia:desktop-tools .
pwsh ./tools/interop.ps1 `
  -EngineImage aimedia:gpu `
  -PeerImage aimedia:test-tools `
  -DesktopImage aimedia:desktop-tools `
  -Suite desktop `
  -SkipToolBuild
```

`obs.py` 通过容器内的 obs-websocket v5 动态创建 Media Source 和自定义推流服务，
不依赖预制的用户场景文件。OBS 输出验收要求实际渲染 PNG，OBS 输入和 VLC
输出验收都要再由 ffprobe 检查媒体包。SRT 的 OBS 配置方式遵循
[OBS 官方 SRT 指南](https://obsproject.com/kb/srt-protocol-streaming-guide)。测试 OBS 的
WebSocket 鉴权只在未暴露端口的临时容器内关闭，不是生产配置示例。

VLC 的 SRT input module 位于 Ubuntu 的 `vlc-plugin-access-extra`，不能只根据
`vlc --version` 判断 SRT 可用。脚本先检查插件文件，再通过 raw dump 保存 aimedia
原始 TS；这样 ffprobe 检查的是引擎时间戳，而不是 VLC 重新 mux 后的时间戳。

延迟输出端回归可以单独运行：

```powershell
pwsh ./tools/interop.ps1 `
  -EngineImage aimedia:gpu `
  -PeerImage aimedia:test-tools `
  -Suite backlog `
  -SkipToolBuild
```

该场景先建立 caller input 并延迟 4 秒连接 output listener，要求视频时间线报告
`capacity=1`、`fullPolicy=backpressure`，视频零丢帧，且 NVDEC surface 高水位不超过
容量。最终输出仍必须从 IDR 开始且 PTS/DTS 单调。

首个组合会在引擎、发送端和接收端的网络命名空间各注入 20ms 延迟、
20ms 抖动和 1% 丢包，形成约 40ms RTT 的双向损伤链路；它需要测试容器的
`NET_ADMIN` capability，不修改宿主机网络。国内构建机
可以显式覆盖测试镜像的 APT 镜像源：

```powershell
pwsh ./tools/interop.ps1 `
  -EngineImage aimedia:gpu `
  -AptMirror http://mirrors.aliyun.com/ubuntu
```

极简 Ubuntu base 默认没有 CA bundle，因此自定义 `APT_MIRROR` 应使用可直接访问的
HTTP Ubuntu 镜像；APT 仍会校验 Ubuntu 签名的 repository metadata 和 package hash。
默认不传参数时使用 Ubuntu 官方源。

脚本在系统临时目录中保留 `summary.json`、输出 TS、OBS 日志和渲染截图；容器和
专用 Docker network 默认自动清理。

RTSP TCP 的外部软件兼容、断源恢复和网络损伤使用单独的短入口；MediaMTX 固定到
1.20.0 image digest，FFmpeg 只生成测试源，真正的解码、节目时钟、编码和 SRT 输出均
由 GPU 运行镜像完成：

```powershell
pwsh ./tools/rtsp.ps1 `
  -EngineImage aimedia:gpu `
  -PeerImage aimedia:test-tools `
  -DurationSeconds 180
```

默认在 45 秒处关闭发布源 8 秒，在 90 秒处向引擎网络命名空间注入 40ms RTT、20ms
抖动和 1% 丢包。报告检查 RTSP 断开状态、404 退避、恢复计数、输出时间戳、TS 连续性、
内部延迟、RSS、GPU surface 与所有队列水位。它是可复现的软件互操作证据，不替代
物理摄像机兼容认证。

RTMP 输入、GPU 转码和 RTMP 输出使用独立门禁。它固定 MediaMTX image digest，要求
真实 FFmpeg publisher 和 consumer，并分别重启输入 publisher 与输出 MediaMTX；随后
向引擎网络命名空间注入 40ms RTT、20ms 抖动和 1% 丢包：

```powershell
pwsh ./tools/rtmp.ps1 `
  -EngineImage aimedia:gpu `
  -PeerImage aimedia:test-tools `
  -DurationSeconds 180
```

报告验证 H.264/AAC profile、两段媒体的逐包时间戳、输出恢复后的首个 keyframe、节目
时钟连续性、输入/输出重连计数、p95 延迟、RSS、GPU surface、队列水位和运行镜像依赖。
短门禁不代替 OBS、真实 RTMPS 平台或两小时 soak。
