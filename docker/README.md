# Docker 镜像

默认根目录 `Dockerfile` 是 CPU/core 镜像，用于配置、TS、回放和 mock control 验证：

```bash
docker build -t aimedia:core .
docker run --rm aimedia:core doctor --json
```

GPU 镜像构建 libsrt 1.5.5 和固定提交的 Android libxaac，并在运行时使用宿主机由
NVIDIA Container Toolkit 注入的驱动库：

```bash
docker build -f docker/Dockerfile.gpu -t aimedia:gpu .
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  aimedia:gpu doctor --strict
```

`doctor --strict` 通过只表示 libsrt、CUDA/NVDEC/NVENC driver libraries 和 libxaac
公共符号可用；在支持矩阵升级前，它不代表 codec 帧处理已经完成。

Video Codec SDK 13.0 的 proprietary archive 不提交到仓库。后续 NVDEC/NVENC frame
binding 构建会要求通过 BuildKit build context 提供 SDK，并在构建脚本中验证版本和哈希。
