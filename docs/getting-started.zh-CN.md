# 快速入门

当前版本可以在 CPU 环境校验配置、编译执行图、运行 fake backend 和检查 native
依赖；Linux + NVIDIA 环境已经闭环单路 NVDEC/NVENC 数据面。每项能力仍以
[支持矩阵](support-matrix.md)为准。

## 1. 先理解当前作业

```text
SRT/MPEG-TS -> TS 拆包 -------\
RTSP/RTP ----> RTP 拆包 -------+-> 压缩音视频包 -> 解码 -> 独立节目时间线
RTMP/FLV ----> FLV tag 转换 ---/                         |
                                                         v
                         编码 -> MPEG-TS/SRT 或 FLV/RTMP(S) 输出
```

v0.2 固定 H.264 8-bit 4:2:0、最高 1080p30、AAC-LC 48kHz 双声道。固定支持范围不是
永久限制，而是为了先把一个真实工作流做完。

不熟悉缩写时先看[术语表](glossary.md)，新架构见[架构说明](architecture.md)。

`MediaJob` 顶层字段可以先按一张订单理解：

| 字段 | 白话含义 | 当前作用 |
|---|---|---|
| `inputs` | 原料从哪里来 | 一到两路输入；v0.3 当前真实数据面只运行单路 |
| `processing` | 原料要加工成什么样 | 视频、音频、时间对齐及可选切换策略 |
| `outputs` | 成品送到哪里 | 配置已经使用列表；当前只允许一个输出，防止误以为 fan-out 已完成 |
| `taps` | 给分析器看的非阻塞样本 | 当前仅保留 `directorSignals` 示例；通用 AI Tap 在 v0.4 |
| `failurePolicy` | 某一步坏了怎么办 | 声明重连、偏差和非关键分析失败行为 |
| `control` | 如何在本机查看和控制作业 | Unix Socket 路径和权限 |

配置先归一化为内部作业，再由图编译器生成唯一的 `ExecutionPlan`。因此旧格式转换和
新格式运行不会形成两套 socket、codec 或 GPU 管线。

v0.3 已把 RTSP TCP interleaved 接入单路运行时。会话边界负责鉴权、SDP 轨道选择和
RTP 拆包，产出的 H.264/AAC-LC/G.711 压缩帧直接进入统一 codec 队列，不会先伪装成
MPEG-TS 再拆一次。G.711 摄像机音频会从 8kHz 单声道桥接为输出需要的 48kHz 双声道。
当前状态仍是 `experimental`：UDP、H.265 bridge、非 48kHz 双声道 AAC、外部摄像机
兼容和长稳分别由后续 V3-02D 至 V3-02F 完成。

RTMP 输入/输出同样直接接公共压缩包边界：listener 把 FLV tag 转为 H.264 Annex-B 和
AAC ADTS，publisher 做反向转换，因此不会为了跨协议发布多做一次 TS 封装/拆包。
RTMPS 校验公开 WebPKI 信任链和主机名；输出断开时不保存历史直播包，重连后等新的
SPS/PPS + IDR 再恢复。外部软件、真实平台和两小时门禁完成前保持 `experimental`。

## 2. 在 CPU 环境查看执行计划

要求 Rust 1.88 或更新版本：

```bash
cargo run -p aimedia -- explain -f examples/single-srt.yaml
cargo run -p aimedia -- explain -f examples/single-srt.yaml --json
cargo run -p aimedia -- explain -f examples/rtsp.yaml
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
完成断流恢复、FFmpeg/OBS/VLC 互操作、网络损伤和两小时稳定性测试，在固定支持范围内
标记为 supported。精确环境与数字见
[v0.2 性能报告](reports/v0.2-native-live-pipe.md)。

RTSP 摄像机使用 `examples/rtsp.yaml` 作为起点，把地址、用户名和密码引用改成真实值；
密码只能来自环境变量或挂载文件。当前输入固定 `transport: tcp`，输出仍是 SRT：

```bash
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  -e AIMEDIA_CAMERA_PASSWORD \
  -v "$PWD/examples:/work:ro" \
  aimedia:gpu run -f /work/rtsp.yaml
```

这是已接通的数据面，不等于已完成市面摄像机兼容认证；正式使用前先以自己的设备做
短时验证，并查看 `control state --json` 中的 `rtsp`、codec 和队列字段。

向支持 RTMPS 的平台发布时，以 `examples/rtmp-output.yaml` 为起点。URI 只写主机和
application 路径，stream key 放在环境变量或挂载文件：

```bash
docker run --rm --gpus all \
  -e NVIDIA_DRIVER_CAPABILITIES=compute,utility,video \
  -e AIMEDIA_RTMP_STREAM_NAME='<stream-key>' \
  -v "$PWD/examples:/work:ro" \
  aimedia:gpu run -f /work/rtmp-output.yaml
```

`control state --json` 的 `output.rtmp` 会报告 `transport`（`tcp` 或 `tls`）、连接状态、
已发送 packet、重连次数和最近发送时间。平台互操作门禁未完成前，先用测试频道或平台
提供的带宽测试入口验证，不能直接把 `experimental` 当成生产兼容承诺。

## 5. 可选导播示例

双输入自动导播不再是核心产品前提，但状态机和回放工具继续作为 Analyzer Tap 与策略
隔离的参考实现：

```bash
cargo run -p aimedia -- replay examples/replay.jsonl -f examples/director.yaml
cargo run -p aimedia -- bench -f examples/director.yaml \
  --capture examples/replay.jsonl --iterations 1000
```

## 6. 常见问题

### 旧配置提示必须转换

`aimedia run` 和 `aimedia explain` 只接受 `aimedia/v1alpha2` 的 `MediaJob`。旧的
`aimedia/v1alpha1` `DirectorPipeline` 不会被静默兼容，先显式转换并检查差异：

```bash
cargo run -p aimedia -- config convert \
  -f examples/v1alpha1.yaml \
  -o /tmp/media-job.yaml
cargo run -p aimedia -- explain -f /tmp/media-job.yaml
```

不传 `-o` 时，新 YAML 输出到标准输出。转换后的 `outputs` 已经是列表结构，但 v0.3
当前数据面仍只接受一个输出；多输出要等 v0.4 完成独立有界分支后才会开放。

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
