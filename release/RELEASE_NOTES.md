## v0.0.2 更新摘要

- **缺陷修复**
  - **P0-1**：修复 `shutdown`/`stop` 响应写回前客户端断开导致跳过销毁的竞态，改为无论写回成败必执行 `zeroize` + `unlink` + `exit`。
  - **Creds JSON 修复**：修复 `build_creds_line` 闭合括号多写一个 `}` 导致 `cred-helper` 解析失败的问题，并补充单测。
- **安全加固**
  - **P1-1**：新增连接读空闲超时 `10s`，异常客户端不再长期占用连接。
  - **P1-2**：新增单行请求长度上限 `64 KiB`，超长拒绝并关闭连接。
  - **P1-3**：`get_pat` 响应采用手工拼接 + `Zeroizing` 缓冲，写出后立即清零。
- **功能变更**
  - **N-1**：项目改名 `gh-pat-daemon`，二进制与 socket/log 统一为 `ghpatd`（breaking）。
  - **N-2**：`set-token` 仅接受 `stdin` 输入，移除文件路径参数。
  - **N-3**：`set-token` 自动兼容 ASCII armored（`age -a`）与二进制密文。
  - **N-4**：统一 `<ISO8601 UTC> action=... result=... peer_pid=...` 审计日志格式。
  - **署名支持**：`start` 支持可选 `--user-name` / `--user-email`，由 `wrap` 自动注入 git 配置。
