# 用户故事

用户故事是路线图的发布门槛。每个故事按“前提—操作—期望—异常”验收；接口骨架
或 mock 通过不代表故事完成。

## 通用集成开发者

**前提**：有一路或两路 OBS、硬件编码器或测试工具产生的 SRT/MPEG-TS
H.264/AAC 流。

**操作**：启动 GPU 容器，运行环境检查和节目配置，并查询 JSON 状态。

**期望**：

- 支持范围内的输入可持续转成一个时间戳单调的节目输出；
- 输出可被 OBS、VLC 和 ffprobe 测试端读取；
- 状态包含 codec 帧数、丢帧、GPU surface、队列水位和 SRT 指标；
- 任何队列都不能通过增加内存掩盖处理不足。

**异常**：缺少 SDK、格式不支持或 GPU 初始化失败时，在建立节目输出前返回带修复
建议的稳定错误；输入断线不积压历史数据。

## 中国大陆直播方案商

**前提**：现场有 RTSP 摄像机或远程 SRT，节目发布目标是阿里云、腾讯云或支持
RTMP/RTMPS 的直播平台。

**操作**：选择横屏或竖屏配置，设置两路输入，通过人工 Take 或自动导播产生节目。

**期望**：

- H.264/H.265 摄像机可作为输入，最终发布 H.264/AAC RTMP/RTMPS；
- 平台控制台看到稳定、连续的节目流；
- 远程 SRT 弱网和本地 RTSP 输入可以使用同一导播与监控模型；
- 鉴权 query、stream key 和摄像机密码不出现在日志。

**异常**：摄像机断流、冻结或时钟偏差超限时，系统拒绝切入坏机位并切到健康备用。
厂商专有协议不可用时必须明确建议通用 RTMP/SRT 接口，不能伪装支持。

## 海外直播工具开发者

**前提**：两路 SRT/RTMP 输入，发布目标为 YouTube、Twitch 或通用 RTMPS endpoint。

**操作**：选择平台预设，通过 secret reference 提供 stream key 并启动节目。

**期望**：

- 输出满足目标平台的 codec、码率、GOP 和音频约束；
- YouTube 非公开直播或 Twitch bandwidth test 能识别节目；
- 平台预设只改变媒体参数，不绕过健康度、同步和人工控制规则。

**异常**：DNS、TLS、鉴权或平台拒绝必须返回不同错误阶段；错误和 debug 输出不得
包含 stream key。

## 海外低延迟应用开发者

**前提**：目标服务提供标准 WHIP endpoint、Bearer token 和可选 STUN/TURN。

**操作**：选择 H.264/Opus WHIP 输出并建立会话。

**期望**：

- 发布链路延迟低于一秒；
- ICE、DTLS、SRTP 和媒体统计可观测；
- WHIP 输出与媒体导播通过有界队列隔离。

**异常**：ICE 重启、TURN 或 SDP 失败只影响对应输出，不能阻塞导播状态机或其他
输出。

## 直播运维人员

**前提**：节目以 Linux Docker + NVIDIA Container Toolkit 部署。

**操作**：观察健康度、codec、网络、同步、队列和控制事件，并注入输入断流、
输出断流及 GPU 错误。

**期望**：

- 输入/输出重连、decoder reset、IDR 请求和故障切换都有计数与原因；
- 网络故障时 RSS/GPU 内存保持有界；
- 健康检查区分依赖可用、codec session 可创建和节目真正 ready。

**异常**：GPU OOM、device lost 或编码器不可恢复错误必须明确终止；禁止静默 CPU
回退或输出损坏码流。

## 初次接触直播开发的用户

**前提**：有 Linux x86_64、Docker、NVIDIA GPU 和示例测试流。

**操作**：按中文快速入门依次执行 `doctor`、`explain`、`run` 和 `control state`。

**期望**：

- 文档从一个可运行的最小配置开始；
- 错误信息指出缺少的 runtime、SDK 文件、端口或 secret；
- 专有名词可跳转到术语表，不要求先掌握 MPEG-TS 或 GPU FFI。

**异常**：不支持的输入不会在运行数分钟后才失败，启动探测必须尽可能提前拒绝。

## 开源贡献者

**前提**：贡献者可能没有 NVIDIA GPU、libsrt、libxaac 或 proprietary SDK。

**操作**：运行默认 Cargo CI、fake backend、replay 和 parser fuzz。

**期望**：

- 默认 workspace 不需要 proprietary SDK；
- native backend 位于 feature 与独立 FFI crate 后；
- FFmpeg 只存在于测试端，不进入产品依赖。

**异常**：启用 GPU feature 但缺少 SDK 时，构建脚本列出缺少的文件、目标 SDK 版本
和 BuildKit context 用法。
