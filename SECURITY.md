# Security Policy

当前 Alpha 仅用于开发和评估，不应直接暴露到不受信任的公网。

安全问题请通过仓库的私密 security advisory 报告，不要公开披露可利用细节。报告应包含受影响版本、最小复现、输入来源和影响判断。

重点攻击面包括 MPEG-TS/H.264/AAC 解析、SRT URI、模型文件、远端 VLM 响应、native codec/GPU FFI 和控制 API。项目不会把 VLM 返回内容当作可执行代码。
