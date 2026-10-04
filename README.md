# ghpatd — 面向云端 AI 智能体的 GitHub PAT 内存代理

Rust 单二进制实现（`x86_64-unknown-linux-musl` 静态链接），让 GitHub PAT 只存在于 `mlock` 内存页、用后即焚，全程不落盘、不进命令行参数与环境变量。

---

## 一、使用说明

### 1. 给云端 AI Agent 的安装与使用 Prompt（直接复制发送给 Agent）

````markdown
请在当前 Linux 环境中安装并使用 `ghpatd`（GitHub PAT 内存代理），按以下步骤操作：

1. 安装 `ghpatd` 到 `$PATH`：
```bash
curl -fsSLO https://github.com/entropy356/gh-pat-daemon/releases/download/v0.0.2/ghpatd-linux-x86_64
curl -fsSLO https://github.com/entropy356/gh-pat-daemon/releases/download/v0.0.2/api.md
install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd || sudo install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd
```
（完整命令与参数参考下载的 `api.md`。）

2. 启动 daemon 并把公钥发给我：
```bash
ghpatd start
# 如需配置 git 提交署名：ghpatd start --user-name "<Name>" --user-email "<Email>"
```
将输出的 `age1...` 公钥发给我，等待我在本地用 `age -r <公钥> -a` 加密 PAT 后把 ASCII armored 密文发给你。

3. 收到我发来的密文后，通过 `stdin` 注入（严禁落盘）：
```bash
ghpatd set-token <<'EOF'
-----BEGIN AGE ENCRYPTED FILE-----
...
-----END AGE ENCRYPTED FILE-----
EOF
ghpatd status
```

4. 日常使用与销毁：
- Git 远程操作：`ghpatd wrap -- git pull`、`ghpatd wrap -- git push`、`ghpatd wrap -- git clone https://github.com/owner/repo.git`
- GitHub 操作：`ghpatd auth status`、`ghpatd repo view`、`ghpatd pr list`、`ghpatd pr create ...`、`ghpatd issue list`、`ghpatd api <endpoint>`
- 任务完成后销毁内存凭据：`ghpatd stop`

5. 安全铁律（必须严格遵守）：
- **严禁尝试获取、读取、打印、记录或导出 PAT 明文**。
- **严禁主动调用 `ghpatd cred-helper get` 或直接向 socket 发送 `get_pat` 请求读取 token**。
- **严禁向我索要 PAT 明文，也严禁将收到的密文或任何凭据写入磁盘文件、环境变量或日志**。
- 只能通过 `ghpatd wrap -- git ...` 或 `ghpatd <repo|pr|issue|api|auth>` 间接使用凭据。
````

### 2. 用户本地加密命令（在本地终端执行）

收到云端 Agent 发来的 `age1...` 公钥后，在本地终端加密 PAT 并复制输出的整段 `-----BEGIN AGE ENCRYPTED FILE----- ... -----END AGE ENCRYPTED FILE-----` 文本发送给 Agent：

```bash
printf '%s' "ghp_xxxxxxxxxxxxxxxxxxxx" | age -r age1xxxxxxxx... -a
```

> 完整命令参数、环境变量与 IPC 协议见 **[docs/API.md](docs/API.md)**。

---

## 二、安全边界

- **核心保证**：
  - `age` 私钥（每次启动重新生成）与 PAT 仅驻留于 daemon 的单页 4KiB `mmap + mlock + MADV_DONTDUMP` 内存页，用后即焚。
  - 启动即设 `prctl(PR_SET_DUMPABLE, 0)` 禁 core dump 与非特权 `ptrace`；`umask(0o077)` + `SO_PEERCRED` 阻断跨 UID 访问。
  - 全程不落盘、不进 `argv`/`env`；日志仅记录脱敏指纹（如 `ghp_…9xYz`），零 PAT 明文。
- **已知边界**：
  - **同 UID 视为授信边界内**：同 UID 进程允许使用凭据与结束进程（通过上述 Prompt 约束 Agent 不主动调用底层凭据接口获取明文）。
  - **外部进程与库缓冲**：`wrap` 交付给 `git` 进程（用户名固定 `x-access-token`）及 `reqwest` 内部的瞬态内存由其自身生命周期管理。
  - **仅限 GitHub HTTPS**：仅向 `https://github.com` / `www.github.com` 提供凭据；`SIGKILL` 强制终止时由内核回收物理内存页。

> 详细内存布局、全链路防护与威胁模型矩阵见 **[docs/SECURITY.md](docs/SECURITY.md)**。

---

## 三、文档与产物

| 路径 | 说明 |
|---|---|
| `ghpatd-linux-x86_64` | Linux x86_64 静态链接二进制（`musl`，`static-pie linked`，`stripped`） |
| `run_tests.sh` | 集成测试脚本（`bash run_tests.sh ./ghpatd-linux-x86_64`） |
| `SHA256SUMS` | 发布产物 SHA-256 校验和 |
| [docs/API.md](docs/API.md) | CLI 命令、参数、环境变量与 Unix Socket IPC 协议参考 |
| [docs/SECURITY.md](docs/SECURITY.md) | 安全架构、敏感内存页模型、威胁模型与安全边界 |
| [docs/REQ.md](docs/REQ.md) | 原始需求说明 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 总体架构与生命周期设计规格 |
| [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) | 逐模块实现规格 |
| [docs/REPLICATION.md](docs/REPLICATION.md) | 复刻步骤、验收清单与集成测试规格 |
