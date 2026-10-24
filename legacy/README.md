# Python 版本归档

`python/` 保存迁移前的 Codex Wiretap 实现和使用说明，包含 HTTP/SSE/WebSocket 监控与原有 Codex skill。此归档保留既有用法；新安装请使用仓库根目录的 Rust 版本和[中文说明](../README.md)或[英文说明](../README.en.md)。运行历史版命令前先进入 `legacy/python/`。

Rust 版本的 BiWrite 号池、账号验证、重试与恢复功能不适用于本归档。Rust 版本中的 CC Switch 兼容监控也是独立实现，不会启动历史 Python 服务。

The `python/` directory preserves the previous implementation, including HTTP/SSE/WebSocket monitoring and the existing Codex skill. New installations should use the Rust edition documented at the repository root. Run historical commands from `legacy/python/`. Rust account pooling, probes, retries, and recovery do not apply to this archive, and Rust CC Switch monitoring does not launch the Python service.
