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
