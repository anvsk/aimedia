# Contributing

感谢参与 aimedia。

- 所有源码必须带可识别的 Apache-2.0 项目许可证边界。
- 不复制 FFmpeg、GStreamer 或其他实现的源码；协议实现以公开规范和独立测试向量为依据。
- 禁止提交模型权重、媒体样本或 codec 代码，除非来源和再分发许可证明确。
- 新 parser 必须有长度上限、固定容量或流式处理策略。
- 新 AI 节点必须声明 deadline、expiry 和故障降级行为。
- Pull request 在提交前运行 `cargo fmt --check`、`cargo check --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`。
- 贡献采用 Developer Certificate of Origin 1.1，提交时使用 `git commit -s`。
