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
释放 libxaac 编码/解码上下文。它仍不代表 NVDEC/NVENC 帧提交或完整直播数据面已经
完成。

## Video Codec SDK 13.0 named context

Video Codec SDK 13.0 的 proprietary archive 和 headers 不提交到仓库。用户必须从
NVIDIA 官方渠道接受许可证、下载并解压 SDK。context 根目录必须直接包含
`Interface/nvEncodeAPI.h`、`Interface/nvcuvid.h` 和 `Interface/cuviddec.h`。

PowerShell 示例：

```powershell
docker buildx build `
  -f docker/Dockerfile.gpu `
  --build-context video_codec_sdk='C:\sdk\Video_Codec_SDK_13.0' `
  --target sdk-runtime `
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
  --target sdk-build-test .
```

SDK headers 和 CUDA headers 只存在于 builder stage，不进入运行镜像。未传 named
context 时，`sdk-runtime` 和 `sdk-build-test` 必须在构建阶段明确失败；普通 CPU CI
以及 `probe-runtime` 不需要 SDK。

GPU 运行镜像将 libsrt 与 libxaac 的许可证和 NOTICE 安装到
`/usr/local/share/licenses/`；SDK headers 不进入最终镜像。

`native-test` 目标还会真实验证 SRT caller/listener 回环和断线恢复：listener 在首个
caller 断开后按配置重建 socket，重连期间统计保持可读，第二个 caller 的消息到达后
`reconnects` 才递增。发送端 adapter 不维护历史消息队列，退避期间的包由上层按有界
队列策略丢弃。
