# ghpatd — 面向云端 AI 智能体的 GitHub PAT 内存代理

Rust 单二进制实现（`x86_64-unknown-linux-musl` 静态链接），让 GitHub PAT 只存在于 `mlock` 内存页、用后即焚，全程不落盘、不进命令行参数与环境变量。

---

## 一、使用说明

> 人类用户可直接复制 **[docs/PROMPT.md](docs/PROMPT.md)** 中的提示词模板发送给云端 Agent。

### 1. 安装

```bash
sha256sum -c SHA256SUMS
install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd
# 若需 root 权限：sudo install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd
```

### 2. 协作注入流程（云端生成公钥 → 本地加密 → 手动复制密文注入）

1. **云端启动 daemon 获取公钥**：
   ```bash
   ghpatd start
   # 可选 git 提交署名：ghpatd start --user-name "AI Agent" --user-email "agent@example.com"
   ```
2. **本地用该 `age1...` 公钥加密 PAT，复制输出的 ASCII armored 密文**：
   ```bash
   printf '%s' "ghp_xxxxxxxxxxxxxxxxxxxx" | age -r age1xxxxxxxx... -a
   ```
3. **云端经 `stdin` 注入密文（不落盘；更换 PAT 时再次执行即可）**：
   ```bash
   ghpatd set-token <<'EOF'
   -----BEGIN AGE ENCRYPTED FILE-----
   ...
   -----END AGE ENCRYPTED FILE-----
   EOF
   ```

### 3. 常用命令

```bash
ghpatd status                                    # 查看运行状态与指纹（READY / ARMED）
ghpatd auth status                               # 验证当前 GitHub 登录身份
ghpatd wrap -- git pull                          # 自动注入 git HTTPS 凭据（及可选署名）执行命令
ghpatd wrap -- git push origin main
ghpatd repo view [owner/repo]                    # 内置 gh 子命令（不依赖 GitHub CLI）
ghpatd pr list [--state open] [--limit 30] [--json <FIELDS>] [--jq <EXPR>]
ghpatd pr create --title "..." --head <branch> --base main [--body "..."]
ghpatd pr merge <number> [--merge|--squash|--rebase]
ghpatd issue list / view / create ...
ghpatd api repos/owner/repo/issues [--method GET] [--field k=v] [--jq <EXPR>]
ghpatd stop                                      # 销毁内存页（zeroize + unlink socket）
```

> 完整命令参数、环境变量与 IPC 协议见 **[docs/API.md](docs/API.md)**。

---

## 二、安全边界

- **核心保证**：
  - `age` 私钥（每次启动重新生成）与 PAT 仅驻留于 daemon 的单页 4KiB `mmap + mlock + MADV_DONTDUMP` 内存页，用后即焚。
  - 启动即设 `prctl(PR_SET_DUMPABLE, 0)` 禁 core dump 与非特权 `ptrace`；`umask(0o077)` + `SO_PEERCRED` 阻断跨 UID 访问。
  - 全程不落盘、不进 `argv`/`env`；日志仅记录脱敏指纹（如 `ghp_…9xYz`），零 PAT 明文。
- **已知边界**：
  - **同 UID 视为授信边界内**：同 UID 进程允许使用凭据与结束进程（应配合 Prompt 约束 Agent 不主动调用底层凭据接口获取明文）。
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
| [docs/PROMPT.md](docs/PROMPT.md) | 供人类复制发送给云端 Agent 的安装与使用 Prompt 模板 |
| [docs/API.md](docs/API.md) | CLI 命令、参数、环境变量与 Unix Socket IPC 协议参考 |
| [docs/SECURITY.md](docs/SECURITY.md) | 安全架构、敏感内存页模型、威胁模型与安全边界 |
| [docs/REQ.md](docs/REQ.md) | 原始需求说明 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 总体架构与生命周期设计规格 |
| [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) | 逐模块实现规格 |
| [docs/REPLICATION.md](docs/REPLICATION.md) | 复刻步骤、验收清单与集成测试规格 |
| [LICENSE](LICENSE) | 开源协议（MIT-0） |
