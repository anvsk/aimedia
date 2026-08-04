# 直播媒体术语表

阅读方式：先看“通俗理解”，再看“在 aimedia 中”。不需要一次记住所有缩写。

| 术语 | 英文全称 | 通俗理解 | 在 aimedia 中 |
| --- | --- | --- | --- |
| MediaJob | Media Job | 一份“我要什么直播结果”的订单 | 声明输入、处理目标、输出、延迟和故障策略，不要求用户手写底层节点 |
| ExecutionPlan | Execution Plan | 开工前生成的施工图 | 图编译器列出具体节点、连接、内存、时钟、队列和资源需求 |
| Graph Compiler | Graph Compiler | 把订单翻译成施工图的规划器 | 校验能力并生成执行计划，本身不打开网络、codec 或 GPU |
| Data plane | Data Plane | 真正搬运和处理音视频的流水线 | 执行接收、拆包、解码、时间线、编码、封装和输出 |
| Control plane | Control Plane | 负责启动、查看、修改和停止流水线的控制室 | 管理作业生命周期和资源准入，不搬运原始视频帧 |
| Hot path | Hot Path | 每一帧都必须及时经过的关键生产线 | AI 和远程服务不能阻塞这条路径 |
| Backpressure | Backpressure | 后面工位处理不过来时，前面必须等待或按规则丢数据 | 每条有界队列明确等待、丢旧、保留最新或失败策略 |
| Memory domain | Memory Domain | 数据当前放在普通内存还是显卡里 | 图计划显式标记 Host 或 NVIDIA Device，减少隐藏复制 |
| Clock domain | Clock Domain | 时间戳属于输入设备、最终节目还是控制事件 | 输入时间只用于映射，输出使用独立节目时钟 |
| Analyzer Tap | Analyzer Tap | 从主流水线旁边取少量样本的观察口 | 给字幕、审核、识别和导播 AI 提供抽样帧、PCM 与指标，失败不影响直播 |
| SRT | Secure Reliable Transport | 像给直播视频准备的一条会补包、能加密的网络专线 | 承载实时输入或输出；可靠传输直接复用 libsrt |
| MPEG-TS | MPEG Transport Stream | 把视频、音频和节目说明切成固定 188 字节小包的“直播运输箱” | SRT 内实际传输的容器；aimedia 独立实现解析和重新封装 |
| PSI | Program Specific Information | 告诉接收端“箱子里有哪些节目和轨道”的目录 | PAT 和 PMT 都属于 PSI，需要周期性发送 |
| PAT | Program Association Table | 总目录：节目编号对应哪一张 PMT | 输出固定指向 PMT PID `0x1000` |
| PMT | Program Map Table | 某个节目的分目录：视频、音频和时钟在哪些 PID | 描述 H.264 视频 PID、AAC 音频 PID 和 PCR PID |
| PES | Packetized Elementary Stream | 把一帧或一段音视频连同时间戳装起来，再切成 TS 包 | 解复用时重组，复用时生成 |
| PID | Packet Identifier | TS 包上的“货架编号” | 区分 PAT、PMT、视频、音频和空包 |
| PTS | Presentation Timestamp | 这一帧应该什么时候显示或播放 | 输入 PTS 只用于同步；输出由独立节目时钟重新生成 |
| DTS | Decoding Timestamp | 解码器应该什么时候开始处理这一帧 | Alpha 禁用 B 帧，因此输出通常等于 PTS |
| PCR | Program Clock Reference | 接收端用来校准播放速度的广播时钟对时信号 | 放在视频 PID，最长 40ms 发送一次 |
| GOP | Group of Pictures | 从一张完整画面到下一张完整画面之间的一组压缩帧 | 输出使用 1 秒 GOP，限制故障恢复和随机接入等待 |
| IDR | Instantaneous Decoder Refresh | 一张能让解码器“从这里重新开始”的完整关键帧 | 切镜和输出重连时请求 NVENC 立即生成 |
| H.264 / AVC | Advanced Video Coding | 常见的视频压缩标准 | Alpha 只支持 8-bit 4:2:0、Main、最高 1080p30 |
| AAC-LC | Advanced Audio Coding, Low Complexity | 常见的有损音频压缩格式 | 只支持 48kHz、双声道；输出 128kbps |
| ADTS | Audio Data Transport Stream | 每个 AAC 音频帧前的小标签，写有长度和采样参数 | TS 中 AAC 的 Alpha 封装格式 |
| PCM | Pulse-Code Modulation | 未压缩的音频采样，可以直接做音量、重采样和 AI 运算 | AAC 解码后的统一音频处理格式；多输入切换不能直接拼接压缩包 |
| NVDEC | NVIDIA Video Decoder | NVIDIA GPU 上专门负责视频解码的硬件单元 | 每个持续处理的输入需要对应解码资源，具体数量写入资源计划 |
| NVENC | NVIDIA Video Encoder | NVIDIA GPU 上专门负责视频编码的硬件单元 | 当前单路节目使用一个编码会话，未来按输出编码配置复用或分配 |
| CUDA | Compute Unified Device Architecture | NVIDIA 的 GPU 编程和内存平台 | 管理 GPU context、NV12 surface 和机位帧复制 |
| NV12 | YUV 4:2:0 semi-planar pixel format | GPU 视频硬件常用的像素排列方式 | NVDEC 输出和 NVENC 输入的 Alpha 内存格式 |
| Surface | — | GPU 中保存一张解码画面的缓冲区 | 用 RAII lease 管理寿命，防止仍在编码时被解码器复用 |
| Jitter buffer | — | 用一点缓存吸收网络“忽快忽慢” | 输入使用固定容量，绝不靠无限增长内存掩盖处理不足 |
| Latency | — | 从摄像机发生事件到观众看到它的总等待 | SRT latency 与 aimedia 引擎新增延迟分开测量 |
| Skew | — | 同一个节目时刻，两路机位实际相差多少 | 超过 `maxSkewMs` 的机位不可切入 |
| Drift | — | 两个时钟速度略有不同，时间差逐渐变大 | 运行时只做缓慢校正，不产生输出时间戳跳变 |
| LUFS | Loudness Units relative to Full Scale | 更接近人耳感知的节目响度刻度 | 两路滚动估计并对齐到约 `-16 LUFS` |
| dBFS | Decibels relative to Full Scale | 数字音频最大幅度为 0 的峰值刻度 | true peak 限制为 `-1 dBFS`，给压缩和播放器留余量 |
| VAD | Voice Activity Detection | 判断“现在有没有人在说话” | 可选 Analyzer Tap，可用于字幕、内容理解或导播策略 |
| VLM | Vision-Language Model | 能看图片并给文字或结构化建议的多模态模型 | 可选慢速 analyzer，永远不能阻塞直播主链 |
| FFI | Foreign Function Interface | Rust 调用 C/C++ 等外部库的边界 | libsrt、NVIDIA SDK、libxaac 都被隔离在独立 crate |
| C ABI | C Application Binary Interface | 不同语言和编译器较容易共同遵守的二进制接口 | 将来的插件接口使用版本化 C ABI，不暴露不稳定 Rust ABI |
| RAII | Resource Acquisition Is Initialization | 对象离开作用域就自动释放资源 | 保证 socket、GPU surface 和 codec handle 不泄漏 |
| caller / listener | — | caller 主动连接，listener 等待对端连接 | 每个 SRT 输入和输出都能显式配置 |
| Stream ID | — | SRT 建连时携带的路由或鉴权字符串 | 可明文配置非敏感路由；包含 token 时必须引用环境变量或文件 |
| Soak test | — | 让系统持续跑很多小时，寻找慢性泄漏和漂移 | PR 阶段 2 小时，阶段发布候选 24 小时 |
