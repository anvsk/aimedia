# 快速入门

当前版本可以在 CPU 环境校验配置、编译执行图、运行 fake backend 和检查 native
依赖；Linux + NVIDIA 环境已经闭环单路 NVDEC/NVENC 数据面。每项能力仍以
[支持矩阵](support-matrix.md)为准。

## 1. 先理解当前作业

```text
SRT/MPEG-TS 输入
        |
        v
拆分音视频 -> 解码 -> 独立节目时间线 -> 编码 -> MPEG-TS/SRT 输出
                         |
                         `-> 非阻塞 AI Tap（后续）
```

v0.2 固定 H.264 8-bit 4:2:0、最高 1080p30、AAC-LC 48kHz 双声道。固定支持范围不是
永久限制，而是为了先把一个真实工作流做完。

不熟悉缩写时先看[术语表](glossary.md)，新架构见[架构说明](architecture.md)。

## 2. 在 CPU 环境查看执行计划

要求 Rust 1.85 或更新版本：

```bash
cargo run -p aimedia -- explain -f examples/single-srt.yaml
cargo run -p aimedia -- explain -f examples/single-srt.yaml --json
```

输出包含：

- 每个接收、拆包、解码、时间线、编码和输出节点；
- 数据位于普通内存还是 NVIDIA 显存；
- 当前使用输入时钟还是独立节目时钟；
- 每条队列的容量和满载策略；
- 节点是已经实现、adapter 已就绪，还是仍然 pending。

只验证配置和图，不打开网络与 GPU：

```bash
cargo run -p aimedia -- run -f examples/single-srt.yaml --dry-run
```

## 3. 检查 Linux GPU 环境

生产目标环境是 Linux x86_64、Docker、NVIDIA GPU、兼容驱动和 NVIDIA Container
Toolkit。GPU 后端构建需要 Video Codec SDK 13.0 ABI headers；参考镜像默认使用固定的
MIT 许可 headers，也支持用户提供 NVIDIA 官方 SDK。

```bash
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  nvidia/cuda:12.8.1-base-ubuntu24.04 nvidia-smi
```

然后运行：

```bash
cargo run -p aimedia -- doctor --json
```

GPU 容器检查失败说明 Docker、驱动或 Toolkit 尚未准备好，不是媒体配置问题。

## 4. 体验有界调度器

mock 模式会运行节目时钟、队列、状态和控制协议，但不会收发真实媒体：

```bash
cargo run -p aimedia -- run -f examples/single-srt.yaml --mock
```

另一个终端查看状态：

```bash
cargo run -p aimedia -- control state --json
```

单输入作业没有切换目标，`take` 和 `auto` 返回 `notApplicable` 是预期行为。

Linux GPU 镜像中，不带 `--mock` 会运行真实单路数据面：

```bash
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  -v "$PWD/examples:/work:ro" \
  aimedia:gpu run -f /work/single-srt.yaml
```

启动前要先准备与配置 caller/listener 方向匹配的 SRT 发送端和接收端。真实数据面已
完成断流恢复、FFmpeg/OBS/VLC 互操作、网络损伤和两小时稳定性测试；在 v0.2 Release
创建前仍标记为 experimental。精确环境与数字见
[v0.2 性能报告](reports/v0.2-native-live-pipe.md)。

## 5. 可选导播示例

双输入自动导播不再是核心产品前提，但状态机和回放工具继续作为 Analyzer Tap 与策略
隔离的参考实现：

```bash
cargo run -p aimedia -- replay examples/replay.jsonl -f examples/director.yaml
cargo run -p aimedia -- bench -f examples/director.yaml \
  --capture examples/replay.jsonl --iterations 1000
```

## 6. 常见问题

### 配置仍然叫 DirectorPipeline

这是 v0.1 配置适配层。新 `MediaJob` 配置会在执行计划接管真实单路链路后引入，避免
现在同时维护两个未闭环运行时。

### 配置提示包含敏感参数

从 URI 删除 `passphrase`、`password`、`token` 或 `secret`，改用 `secretRef.env` 或
`secretRef.file`。

### 找不到 NVIDIA SDK 或 NVDEC 驱动库

图编译器、CPU 测试和 fake backend 不需要 SDK。启用 NVIDIA codec feature 时，按
[GPU 镜像说明](../docker/README.md)选择固定的 `nv-codec-headers`，或通过 named context
传入官方 SDK 13.0；两类头文件都不会提交到仓库或复制进运行镜像。

容器运行参数还必须包含
`NVIDIA_DRIVER_CAPABILITIES=compute,utility,video`。缺少 `video` 时，即使
`nvidia-smi` 正常，容器中仍可能没有 NVDEC/NVENC 驱动库。

### Windows workspace 检查失败

真实 transport 与 GPU 基线是 Linux。Windows 可运行纯 Rust 的图和 parser 测试；完整
workspace 和 native 数据面使用 Linux/Docker CI 验证。
