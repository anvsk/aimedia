# 直播媒体术语表

阅读方式：先看“通俗理解”，再看“在 aimedia 中”。不需要一次记住所有缩写。

| 术语 | 英文全称 | 通俗理解 | 在 aimedia 中 |
| --- | --- | --- | --- |
| SRT | Secure Reliable Transport | 像给直播视频准备的一条会补包、能加密的网络专线 | 承载两路机位输入和一路节目输出；可靠传输直接复用 libsrt |
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
| PCM | Pulse-Code Modulation | 未压缩的音频采样，可以直接做音量和淡化运算 | 两路 AAC 持续解码为 PCM 后再切换，不能拼接压缩包 |
| NVDEC | NVIDIA Video Decoder | NVIDIA GPU 上专门负责视频解码的硬件单元 | 两个机位各保留一个持续工作的解码会话 |
| NVENC | NVIDIA Video Encoder | NVIDIA GPU 上专门负责视频编码的硬件单元 | 整个节目只用一个编码会话，确保输出连续 |
| CUDA | Compute Unified Device Architecture | NVIDIA 的 GPU 编程和内存平台 | 管理 GPU context、NV12 surface 和机位帧复制 |
| NV12 | YUV 4:2:0 semi-planar pixel format | GPU 视频硬件常用的像素排列方式 | NVDEC 输出和 NVENC 输入的 Alpha 内存格式 |
| Surface | — | GPU 中保存一张解码画面的缓冲区 | 用 RAII lease 管理寿命，防止仍在编码时被解码器复用 |
| Jitter buffer | — | 用一点缓存吸收网络“忽快忽慢” | 每路固定容量，过满淘汰旧数据，绝不无限增长 |
| Latency | — | 从摄像机发生事件到观众看到它的总等待 | SRT latency 与 aimedia 引擎新增延迟分开测量 |
| Skew | — | 同一个节目时刻，两路机位实际相差多少 | 超过 `maxSkewMs` 的机位不可切入 |
| Drift | — | 两个时钟速度略有不同，时间差逐渐变大 | 运行时只做缓慢校正，不产生输出时间戳跳变 |
| LUFS | Loudness Units relative to Full Scale | 更接近人耳感知的节目响度刻度 | 两路滚动估计并对齐到约 `-16 LUFS` |
| dBFS | Decibels relative to Full Scale | 数字音频最大幅度为 0 的峰值刻度 | true peak 限制为 `-1 dBFS`，给压缩和播放器留余量 |
| VAD | Voice Activity Detection | 判断“现在有没有人在说话” | 第三阶段快脑信号，本阶段不进入实时链路 |
| VLM | Vision-Language Model | 能看图片并给文字或结构化建议的多模态模型 | 只做慢速建议，永远不能阻塞直播主链 |
| FFI | Foreign Function Interface | Rust 调用 C/C++ 等外部库的边界 | libsrt、NVIDIA SDK、libxaac 都被隔离在独立 crate |
| C ABI | C Application Binary Interface | 不同语言和编译器较容易共同遵守的二进制接口 | 将来的插件接口使用版本化 C ABI，不暴露不稳定 Rust ABI |
| RAII | Resource Acquisition Is Initialization | 对象离开作用域就自动释放资源 | 保证 socket、GPU surface 和 codec handle 不泄漏 |
| caller / listener | — | caller 主动连接，listener 等待对端连接 | 每个 SRT 输入和输出都能显式配置 |
| Stream ID | — | SRT 建连时携带的路由或鉴权字符串 | 可明文配置非敏感路由；包含 token 时必须引用环境变量或文件 |
| Soak test | — | 让系统持续跑很多小时，寻找慢性泄漏和漂移 | PR 阶段 2 小时，阶段发布候选 24 小时 |
