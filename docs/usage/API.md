# ghpatd API 与命令行参考文档

本文档涵盖 `ghpatd` 的三种运行形态、CLI 子命令参数、环境变量、Git Credential Helper 协议及 Unix Socket IPC 协议规格。

---

## 1. 运行形态与入口分叉

`ghpatd` 为单二进制程序，启动时严格按 `argv[1]` 分叉（不依赖环境变量判断形态）：

| 形态 | 触发条件 | 说明 |
|---|---|---|
| `client` | `argv[1]` 为常规子命令 | 用户/智能体交互入口，经 Unix Socket 与 daemon 通信 |
| `cred-helper` | `argv[1] == "cred-helper"` | 供 `git` 调用的凭据辅助程序，实现 git credential helper 协议 |
| `daemon` | `argv[1] == "--daemon-internal"` | 后台常驻守护进程，持有 `mlock` 敏感内存页并执行 GitHub REST 请求 |

---

## 2. 全局选项与环境变量

### 2.1 全局 CLI 选项

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--sock <PATH>` | `${XDG_RUNTIME_DIR:-/tmp/ghpatd-$UID}/ghpatd.sock` | 覆盖 Unix Socket 路径（支持置于任意子命令前后） |
| `-h, --help` | — | 打印帮助信息 |
| `-V, --version` | — | 打印版本号（`ghpatd 0.0.2`） |

### 2.2 环境变量

| 变量名 | 作用域 | 说明 |
|---|---|---|
| `GHPATD_SOCK` | `client` / `cred-helper` / `daemon` | 指定 socket 路径（优先级低于 `--sock`，高于 `XDG_RUNTIME_DIR`） |
| `XDG_RUNTIME_DIR` | `client` / `cred-helper` | 未指定 `--sock` 与 `GHPATD_SOCK` 时，默认使用 `$XDG_RUNTIME_DIR/ghpatd.sock` |
| `GH_REPO` | `client`（`repo`/`pr`/`issue`） | 默认目标仓库（`OWNER/REPO`），优先级低于 `-R/--repo`，高于本地 `git remote` |

---

## 3. 生命周期与凭据管理命令

### `ghpatd start`

启动后台 daemon 进程，在内存内生成一次性 `age` X25519 密钥对并打印公钥。

```bash
ghpatd start [--foreground] [--user-name <NAME>] [--user-email <EMAIL>]
```

- `--foreground`：不 `fork`，在当前进程直接进入 daemon 事件循环（调试用）。
- `--user-name <NAME>`：可选，设置 git 提交署名 `user.name`（存于 daemon 内存，由 `wrap` 自动注入）。
- `--user-email <EMAIL>`：可选，设置 git 提交署名 `user.email`（存于 daemon 内存，由 `wrap` 自动注入）。
- **标准输出**：
  ```text
  age1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
  daemon 已启动 (pid 1234, socket: /tmp/ghpatd-1000/ghpatd.sock)
  状态: READY（等待 token 注入）
  ```
- **退出码**：成功 `0`；已在运行（`DAEMON_ALREADY_RUNNING`）或超时/失败 `1`。

### `ghpatd set-token`

从标准输入（`stdin`）读取经当前 daemon `age` 公钥加密的 PAT 密文，解密并调用 `GET https://api.github.com/user` 校验通过后写入 `mlock` 敏感内存页。支持更换已有 PAT。

```bash
ghpatd set-token <<'EOF'
-----BEGIN AGE ENCRYPTED FILE-----
...
-----END AGE ENCRYPTED FILE-----
EOF
```

- **输入格式**：仅接受 `stdin`（不接受文件路径参数），自动兼容 ASCII armored（`age -a`）与二进制密文。
- **失败语义**：若密文损坏、公钥不匹配、使用密码短语加密（非 `-r` 公钥模式）或 GitHub 返回 `401`，注入被拒绝，**daemon 原有状态与已注入的旧 PAT 保持不变**。
- **退出码**：成功 `0`；失败 `1`。

### `ghpatd status`

查询 daemon 当前状态与已注入 PAT 的脱敏指纹。

```bash
ghpatd status
# 未注入：状态: READY（等待 token 注入）
# 已注入：状态: ARMED (fingerprint: ghp_…9xYz)
```

### `ghpatd pubkey`

仅输出当前运行中 daemon 的 `age1...` 公钥字符串（便于脚本化获取）。

```bash
ghpatd pubkey
```

### `ghpatd stop`

通知 daemon 立即销毁敏感内存页（`write_volatile` 全页写零）、删除 socket 文件并退出。

```bash
ghpatd stop
```

---

## 4. Git 包装与凭据助手

### `ghpatd wrap`

在临时注入 Git 凭据与署名配置的环境中执行任意目标命令：

```bash
ghpatd wrap -- <COMMAND> [ARGS...]
```

注入的子进程环境变量（支持嵌套 `wrap`，在既有 `GIT_CONFIG_COUNT` 偏移上累加）：

| 变量 | 值 |
|---|---|
| `GIT_CONFIG_COUNT` | `existing + 1 + N`（`N` 为已配置的署名项数：`0`/`1`/`2`） |
| `GIT_CONFIG_KEY_<idx>` | `credential.https://github.com.helper` |
| `GIT_CONFIG_VALUE_<idx>` | `!<ghpatd绝对路径> cred-helper` |
| `GIT_CONFIG_KEY_<idx+1>` | `user.name`（当 `start` 指定了 `--user-name` 时） |
| `GIT_CONFIG_KEY_<idx+2>` | `user.email`（当 `start` 指定了 `--user-email` 时） |
| `GIT_TERMINAL_PROMPT` | `0`（禁止 git 退化为交互式密码提示） |
| `GHPATD_SOCK` | 当前 socket 路径 |

- **退出码**：原样透传目标命令退出码；目标命令无法启动时返回 `127`。

### `ghpatd cred-helper`

实现 Git Credential Helper 协议，通常由 `git` 自动调用：

```bash
ghpatd cred-helper <get|store|erase>
```

- `get`：从 `stdin` 读取 `protocol=` 与 `host=` 直至空行：
  - 仅当 `protocol == "https"` 且 `host ∈ {"github.com", "www.github.com"}` 时向 daemon 请求 `get_pat`，成功则向 `stdout` 输出：
    ```text
    username=x-access-token
    password=<PAT>

    ```
  - 其他域名、协议、未注入 token 或 daemon 未运行时，静默输出空内容并以退出码 `0` 返回。
- `store` / `erase`：静默忽略并返回退出码 `0`，不落盘任何凭据。

---

## 5. 内置 GitHub CLI 子命令与 API 透传

所有内置子命令均由 daemon 在进程内直接发起 HTTPS 请求访问 `https://api.github.com`（PAT 不离开 daemon 进程）。

> **目标仓库解析顺序**：`-R / --repo <OWNER/REPO>` > 环境变量 `GH_REPO` > 当前工作目录 `git remote get-url origin`（仅支持 `https://github.com/owner/repo(.git)`）。

### 5.1 认证与仓库

```bash
# 校验当前 PAT 并打印登录名（已认证为 <login>）
ghpatd auth status

# 查看仓库详情（名称、描述、可见性、默认分支、Stars、更新时间、URL）
ghpatd repo view [OWNER/REPO] [-R/--repo OWNER/REPO]

# 列出当前用户有权限的仓库（按更新时间排序，表格列：NAME / DESCRIPTION / UPDATED）
ghpatd repo list [--limit 30]
```

### 5.2 Pull Request 操作

```bash
# 列出 PR（表格列：NUMBER / TITLE / BRANCH / STATE）
ghpatd pr list [--state open|closed|all] [--limit 30] [-R/--repo OWNER/REPO]

# 查看指定 PR（title / state / author / branch / url）
ghpatd pr view <NUMBER> [-R/--repo OWNER/REPO]

# 创建 PR
ghpatd pr create --title <TITLE> --head <HEAD> --base <BASE> [--body <BODY>] [-R/--repo OWNER/REPO]

# 合并 PR（默认 merge）
ghpatd pr merge <NUMBER> [--merge | --squash | --rebase] [-R/--repo OWNER/REPO]
```

### 5.3 Issue 操作

```bash
# 列出 Issue（自动过滤 PR；表格列：NUMBER / TITLE / STATE）
ghpatd issue list [--state open|closed|all] [--limit 30] [-R/--repo OWNER/REPO]

# 查看指定 Issue（title / state / author）
ghpatd issue view <NUMBER> [-R/--repo OWNER/REPO]

# 创建 Issue
ghpatd issue create --title <TITLE> [--body <BODY>] [-R/--repo OWNER/REPO]
```

### 5.4 REST API 透传与 `jq` 过滤

```bash
ghpatd api <ENDPOINT> [--method GET|POST|PUT|PATCH|DELETE] [--field KEY=VALUE]... [--jq <EXPR>]
```

- `<ENDPOINT>`：如 `user`、`repos/owner/repo/issues`（前导 `/` 可省）。
- `--field KEY=VALUE`：`GET`/`DELETE` 时编码为 URL query 参数；`POST`/`PUT`/`PATCH` 时构造为 JSON 请求体（值若为合法 JSON 字面量如数字/布尔值则按 JSON 类型解析，否则按字符串处理）。
- `--jq <EXPR>`：使用内置 `jaq` 引擎在内存内对响应 JSON 执行 jq 表达式过滤，无需安装外部 `jq` 命令。

---

## 6. Unix Socket IPC 协议

- **传输层**：Unix Domain Stream Socket（默认 `${XDG_RUNTIME_DIR:-/tmp/ghpatd-$UID}/ghpatd.sock`）
- **编码格式**：UTF-8 JSON Lines（每条请求/响应占单行，以 `\n` 结尾）
- **防护限制**：连接读空闲超时 `10s`，单行请求最大长度 `65536` 字节（64 KiB），`SO_PEERCRED` 强制校验同 UID

### 6.1 请求与命令集

```json
{"id": 1, "cmd": "<命令>", "enc_b64": "...", "args": ["..."], "repo": "owner/repo", "host": "github.com", "protocol": "https"}
```

| `cmd` | 必填字段 | 成功响应 `payload` | 说明 |
|---|---|---|---|
| `status` | — | `{"state": "ready"\|"armed", "fingerprint": "ghp_…9xYz"\|null}` | 查询运行状态与指纹 |
| `pubkey` | — | `{"pubkey": "age1..."}` | 获取当前 age 公钥 |
| `getuser` | — | `{"user": "..."\|null, "email": "..."\|null}` | 获取 `start` 时配置的 git 署名 |
| `set_token` | `enc_b64` | `{"login": "...", "scopes": ["..."], "fingerprint": "..."}` | 传入 base64 编码的 age 密文并校验注入 |
| `get_pat` | `host`, `protocol` | `{"username": "x-access-token", "password": "<PAT>"}` | 仅供 `cred-helper` 获取凭据（走 `Zeroizing` 手工序列化） |
| `gh` | `args`（可选 `repo`） | `{"stdout": "...", "stderr": "...", "exit_code": 0}` | 在 daemon 内执行 `gh`/`api` 子命令 |
| `shutdown` | — | `{"ok": true}` | 写回响应后立即清零敏感页、删除 socket 并退出进程 |

### 6.2 错误码清单

| 错误码 (`error.code`) | 触发条件 | 客户端标准提示文案 |
|---|---|---|
| `DAEMON_NOT_RUNNING` | Socket 不存在或无法连接 | `✘ daemon 未运行，请先执行 ghpatd start` |
| `DAEMON_ALREADY_RUNNING` | `start` 时探测到已有存活 daemon | `✘ daemon 已在运行（DAEMON_ALREADY_RUNNING）` |
| `START_TIMEOUT` | `start` 等待子进程就绪管道超过 5s | `✘ daemon 启动超时，exit(1)` |
| `NO_TOKEN` | 未注入 PAT 时调用 `gh` 或 `get_pat` | `✘ PAT 未注入，请执行 ghpatd set-token（stdin）` |
| `NOT_RECIPIENT_FORMAT` | 密文为 passphrase 模式而非 `-r` 公钥加密 | `✘ 仅支持 age -r 公钥加密（NOT_RECIPIENT_FORMAT）` |
| `DECRYPT_FAILED` | 密文损坏、公钥不匹配或明文为空/非 UTF-8 | `✘ 解密失败：密文与公钥不匹配或已损坏` |
| `PAT_TOO_LONG` | 解密后 PAT 超过 256 字节上限 | `✘ PAT_TOO_LONG: PAT 超长（>256 字节）` |
| `TOKEN_INVALID` | `set-token` 校验 `GET /user` 返回 401 | `✘ token 无效（401），注入被拒绝；保留原状态` |
| `API_ERROR` | GitHub API 网络错误或返回非预期状态码 | `✘ API_ERROR: ...` |
| `HOST_NOT_ALLOWED` | `get_pat` 非 `https` 或非 `github.com` 域 | `✘ HOST_NOT_ALLOWED: 不为 <host> 代理凭据` |
| `REMOTE_NOT_HTTPS` | 本地 git `origin` 为 SSH URL 而非 HTTPS | `✘ ghpatd 仅支持 HTTPS remote` |
| `BAD_REQUEST` | JSON 格式非法、未知命令或单行超过 64 KiB | `✘ BAD_REQUEST: ...` |
