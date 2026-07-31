# 第二阶段快速入门

当前仓库正在按路线图接通原生数据面。每条命令是否可用，以 `README` 的“当前状态”和支持矩阵为准。

## 1. 先理解要运行什么

```text
机位 wide  --SRT--> \
                      aimedia --SRT--> 节目播放器或平台
机位 close --SRT--> /
```

配置允许一路或两路输入。v0.2 优先接通单路闭环；两路导播数据面属于 v0.3。输入
必须是 MPEG-TS，视频为 H.264 8-bit 4:2:0、最高 1080p30，音频为 AAC-LC
48kHz 双声道；两路模式还要求分辨率和帧率相同。

不熟悉缩写时先看[术语表](glossary.md)，架构取舍见[设计理由](design-rationale.md)。

## 2. 环境要求

- Linux x86_64。
- Docker Engine 或 Docker Desktop。
- NVIDIA GPU、兼容驱动和 NVIDIA Container Toolkit。
- Video Codec SDK 13.0，仅在构建 GPU 后端时需要。

先检查 GPU 是否能进入容器：

```bash
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  nvidia/cuda:12.8.1-base-ubuntu24.04 nvidia-smi
```

这一步失败表示 Docker/GPU 环境还没有准备好，不是 SRT 或导播配置问题。

## 3. 准备配置

单路从 `examples/single-srt.yaml` 开始；体验双路控制协议时复制
`examples/director.yaml`。修改 SRT URI 后，密码不能直接写入 YAML：

```bash
export AIMEDIA_SRT_WIDE_PASSPHRASE='replace-me'
export AIMEDIA_SRT_CLOSE_PASSPHRASE='replace-me'
export AIMEDIA_SRT_OUTPUT_PASSPHRASE='replace-me'
```

先只检查配置和图：

```bash
aimedia explain -f examples/single-srt.yaml
aimedia run -f examples/single-srt.yaml --dry-run
```

单路配置的 `state --json` 返回 `mode: single`。此模式没有选镜对象，`take` 和
`auto` 会稳定返回 `notApplicable`，这不是输入故障。

## 4. 启动当前可用的调度器和人工切镜

当前先用 mock 模式体验节目时钟、状态和控制协议；它不会收发媒体：

```bash
aimedia run -f director.yaml --mock
```

另开一个终端查看状态或切镜：

```bash
aimedia control state --json
aimedia control take --input close --hold-ms 5000
aimedia control auto
```

`hold-ms 0` 表示保持人工模式，直到执行 `auto`。不可用或不同步的目标机位会返回 `targetUnavailable`。

当前不带 `--mock` 的 `aimedia run` 会明确退出。libxaac 帧级处理已经完成，但
NVDEC/NVENC 和 codec 到 scheduler 的真实数据面还没有接通。后续支持真实流时，
本节才会替换为生产启动命令。

## 5. 常见问题

### 配置提示包含敏感参数

从 SRT URI 删除 `passphrase`、`password`、`token` 或 `secret`，改用 `secretRef.env` 或 `secretRef.file`。

### Take 被拒绝

运行 `aimedia control state --json`，检查目标机位的 `healthy`、`synchronized`、`frozen` 和 `skewMs`。偏差超过配置上限时拒绝切入是保护行为。

### 设计中的“两路都断开”行为

实时数据面接通后，管线应保持最后一张健康画面并输出静音，同时尝试重连；若输出端
持续不可恢复或 codec/GPU 失败，进程应明确退出。当前 mock 模式不能验证这一行为。

### 找不到 NVIDIA SDK

核心和 CPU 测试不需要 SDK。只有启用 `nvidia` 特性时才需要按构建说明提供 SDK 13.0；SDK 压缩包和头文件不会提交到项目仓库。
