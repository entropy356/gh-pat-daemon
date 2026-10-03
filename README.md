# ghpatd — 面向云端 AI 智能体的 GitHub PAT 内存代理

单二进制实现（Rust，`x86_64-unknown-linux-musl` 静态链接），在一个可执行文件内通过 `argv` 分叉实现 `client`、`daemon (--daemon-internal)` 与 `cred-helper` 三种形态。PAT 全生命周期仅驻留于 daemon 进程的 `mlock` 内存页，用后即焚，不落盘、不进命令行参数与环境变量。

---

## 一、使用说明

### 1. 安装

将静态二进制 `ghpatd-linux-x86_64` 安装到 `$PATH` 下的 `ghpatd`：

```bash
sha256sum -c SHA256SUMS
install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd
# 若当前用户无 /usr/local/bin 写权限：
sudo install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd
```

### 2. 云端 Agent 协作注入流程（手动复制密文）

#### 第一步：【云端 Agent】启动 daemon 并获取一次性公钥

每次启动重新在内存内生成 `age` 密钥对（私钥永不落盘、不跨运行复用）：

```bash
ghpatd start
# 输出示例：
# age1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
# daemon 已启动 (pid 1234, socket: /tmp/ghpatd-1000/ghpatd.sock)
# 状态: READY（等待 token 注入）

# 可选配置 git 提交署名（存于 daemon 内存，由 wrap 自动注入）：
ghpatd start --user-name "AI Agent" --user-email "agent@example.com"
```

云端 Agent 将输出的 `age1...` 公钥发给用户。

#### 第二步：【用户本地】用公钥加密 PAT 并复制密文给 Agent

用户在**本地终端**使用该公钥将 PAT 加密为 ASCII armored 文本（PAT 明文仅留在本地）：

```bash
# 方式 A：标准输入直接加密（本地不落盘明文文件）
printf '%s' "ghp_xxxxxxxxxxxxxxxxxxxx" | age -r age1xxxxxxxx... -a

# 方式 B：从本地已有文件加密输出到终端
age -r age1xxxxxxxx... -a pat.txt
```

复制终端输出的整段 `-----BEGIN AGE ENCRYPTED FILE----- ... -----END AGE ENCRYPTED FILE-----` 文本密文，粘贴发送给云端 Agent。

#### 第三步：【云端 Agent】经 stdin 注入密文（不落盘）

云端 Agent 将收到的密文通过 heredoc 直接喂给 `set-token`（daemon 在内存内解密并调用 `GET /user` 校验，通过后写入 `mlock` 敏感页，状态变为 `ARMED`；更换 PAT 时再次执行 `set-token` 即可）：

```bash
ghpatd set-token <<'EOF'
-----BEGIN AGE ENCRYPTED FILE-----
age-encryption.org/v1
-> X25519 ...
...
-----END AGE ENCRYPTED FILE-----
EOF
```

### 3. 日常命令

```bash
# 状态与公钥
ghpatd status                                    # 查看运行状态与 PAT 指纹（READY / ARMED）
ghpatd pubkey                                    # 打印当前 daemon 的 age 公钥
ghpatd auth status                               # 校验当前 token 对应的 GitHub 账号与 scopes

# Git 操作（自动注入 HTTPS credential helper 及可选 user.name / user.email）
ghpatd wrap -- git clone https://github.com/owner/repo.git
ghpatd wrap -- git pull
ghpatd wrap -- git commit -m "feat: update"
ghpatd wrap -- git push origin main

# 内置 gh 子命令（不依赖 GitHub CLI，由 daemon 直接调用 GitHub REST API）
ghpatd repo view [owner/repo]
ghpatd repo list --limit 30
ghpatd pr list [--state open] [--limit 30] [--json] [--jq <EXPR>]
ghpatd pr view <number> [--json] [--jq <EXPR>]
ghpatd pr create --title "..." --head <branch> --base main [--body "..."]
ghpatd pr merge <number> [--merge|--squash|--rebase]
ghpatd issue list [--state open] [--limit 30] [--json] [--jq <EXPR>]
ghpatd issue view <number> [--json] [--jq <EXPR>]
ghpatd issue create --title "..." [--body "..."]
ghpatd api repos/owner/repo/issues [--method GET] [--field k=v] [--jq <EXPR>]

# 销毁退出（volatile 写零整页 mlock 内存 + unlink socket）
ghpatd stop
```

---

## 二、安全边界

### 1. 防护保证（威胁模型内）

- **用后即焚、零落盘**：`age` 私钥与 PAT 仅驻留于 daemon 进程的单页 4KiB `mmap(MAP_PRIVATE|MAP_ANONYMOUS)` + `mlock` + `MADV_DONTDUMP` 内存页；`set-token` 仅从 `stdin` 读取密文，任何情况下不落盘、不进命令行参数（`argv`）与环境变量（`env`）。
- **禁 Core Dump 与非特权附加**：启动时最先执行 `prctl(PR_SET_DUMPABLE, 0)`，阻止产生 core dump 及无 `CAP_SYS_PTRACE` 的进程 `ptrace`。
- **跨 UID 隔离**：`umask(0o077)` 先于 `bind`，确保 `ghpatd.sock` 与 `ghpatd.log` 创建即 `0600`、运行目录 `0700`；accept 后通过 `SO_PEERCRED` 校验连接方 UID，跨 UID 进程不可读取 PAT、不可注入命令。
- **瞬态副本收敛与原子替换**：IPC `get_pat` 响应采用手工 JSON 拼接 + `Zeroizing` 缓冲写出即清零；更换 PAT 时原地 `write_volatile` 抹除旧值，若新密文解密或 `GET /user` 校验失败则完全保留原状态；`stop`/`shutdown` 即使客户端提前断开也必完成清零与销毁。
- **日志零明文**：审计日志仅记录脱敏指纹（如 `ghp_…9xYz` / `github_pat_…9xYz`），任何情况下不包含 PAT 明文。

### 2. 边界与已知限制

- **同 UID 进程视为授信边界内**：云端 Agent 与 `ghpatd` 运行在同一 UID 下，允许其通过子命令使用凭据或结束进程；协议层不额外鉴权同 UID 的 `cred-helper get` / `shutdown` 调用（防止无意落盘与环境残留泄露，而非对抗同 UID 恶意主动窃取）。
- **宿主进程与第三方 HTTP 库内存**：`wrap` 场景下凭据按 git credential 协议交付给本机 `git` 进程（用户名固定 `x-access-token`）；内置 `gh` 子命令经 `reqwest` 发起 HTTPS 请求时，第三方库内部瞬态缓冲无法保证 `zeroize`。
- **仅限 GitHub HTTPS**：仅向 `https://github.com` 与 `https://www.github.com` 提供凭据，不支持 SSH remote。
- **强制杀死（`SIGKILL`）**：进程被 `kill -9` 强制终止时无法执行用户态钩子，由操作系统内核直接回收 `mlock` 匿名内存页；下次 `ghpatd start` 会自动探测并清理残留 socket。

---

## 三、文档与产物索引

| 文件 | 说明 |
|---|---|
| `ghpatd-linux-x86_64` | Linux x86_64 静态链接二进制（`musl`，`static-pie linked`，`stripped`） |
| `run_tests.sh` | 集成自测脚本（`bash run_tests.sh ./ghpatd-linux-x86_64`） |
| `SHA256SUMS` | 发布产物 SHA-256 校验和 |
| [docs/REQ.md](docs/REQ.md) | 原始需求说明 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 总体架构、IPC 协议、敏感页内存模型、生命周期、威胁模型 |
| [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) | 逐模块实现规格（数据结构、函数签名、行为规则、常量与算法） |
| [docs/REPLICATION.md](docs/REPLICATION.md) | 复刻步骤、验收清单、集成测试规格、安全自查清单 |
