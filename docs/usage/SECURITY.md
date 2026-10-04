# ghpatd 安全设计、威胁模型与边界文档

本文档阐述 `ghpatd` 的安全设计目标、内存保护机制、密钥与凭据生命周期、IPC 防护、威胁模型矩阵及已知安全边界。

---

## 1. 安全定位与核心目标

`ghpatd` 面向运行在无 root Linux 虚拟机/沙箱中的云端 AI 智能体，核心目标是实现 **GitHub PAT 只驻留内存、用后即焚**：

1. **端到端密文注入**：人类用户在本地终端用 `ghpatd` 生成的一次性 `age` 公钥加密 PAT，仅将 ASCII armored 密文复制发送给云端智能体；PAT 明文从不出现在聊天上下文、脚本文件、命令行参数或持久化存储中。
2. **内存隔离与用后即焚**：`age` 私钥与 PAT 在云端仅驻留于 daemon 进程受保护的单个匿名 `mlock` 内存页，进程结束、收到终止信号或更换 token 时立即覆写清零。
3. **防止无意泄露**：消除 `~/.git-credentials`、`GITHUB_TOKEN` 环境变量、`ps -eo args`、core dump、swap 交换分区及构建/运行日志中的凭据残留风险。

---

## 2. 敏感内存页模型（`SensitivePage`）

daemon 进程内唯一权威敏感状态存储于 `SensitivePage`（`ghpatd/src/page.rs`）：

### 2.1 页分配与内核锁页

启动时通过系统调用一次性分配单页 4096 字节（4 KiB）：

```text
mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
  └─▶ mlock(ptr, 4096)                 // 锁定物理内存页，禁止内核换出到 swap
  └─▶ madvise(ptr, 4096, MADV_DONTDUMP) // 核心转储时排除该页
```

- 任一步骤失败立即 `munmap` 并报错退出，绝不降级运行在普通堆内存上。
- 进程存活期间该页**永不 `munlock`、永不 `realloc`**，规避通用堆分配器因扩缩容产生旧副本残留或同页其他对象释放导致误解锁的陷阱。

### 2.2 页内定长布局与 `volatile` 读写

| 字节偏移 | 长度 | 内容 |
|---|---|---|
| `0..32` | 32 B | `age` X25519 私钥原始字节（每次 `start` 重新生成，不跨运行复用） |
| `32..40` | 8 B | `pat_len`（`u64` 小端序，当前有效 PAT 字节长度） |
| `40..296` | 256 B | `pat_buf`（PAT 明文定长字节区，上限 `PAT_CAP = 256`） |
| `296..4096` | 3800 B | 预留区（保持全零填充） |

- 所有页内写入与清零均逐字节调用 `core::ptr::write_volatile` / `read_volatile`，防止编译器将销毁前的清零操作当作“死存储（Dead Store）”优化消除。
- **更换 PAT（`set_pat`）**：先以 `write_volatile` 将 `max(旧长度, 新长度)` 范围及 `pat_len` 全部写零，再写入新 PAT。
- **销毁（`zeroize_all` / `Drop`）**：整页 4096 字节 `write_volatile(0)` 后再执行 `munlock` 与 `munmap`。

---

## 3. 启动、注入、取用与销毁全链路防护

### 3.1 启动序列（严格次序）

`ghpatd start` 先 `fork` 子进程并通过 `exec` 进入 `--daemon-internal`，子进程严格按以下顺序初始化：

1. `umask(0o077)`：先于 `bind()` 设置，保证随后创建的 `ghpatd.sock` 与 `ghpatd.log` 落地瞬间即为 `0600`，彻底消除 `bind` → `chmod` 之间的 TOCTOU 竞态窗口。
2. `prctl(PR_SET_DUMPABLE, 0)`：在生成任何密钥数据前关闭进程可转储属性，禁止产生 core dump，并阻止无 `CAP_SYS_PTRACE` 的同 UID 进程通过 `ptrace` 或 `/proc/<pid>/mem` 附加读取。
3. 分配并锁定 `SensitivePage`。
4. 生成一次性 `age` 密钥对，私钥原始字节写入 `SensitivePage`，公钥经匿名管道回传给父进程打印。

### 3.2 密文注入与原子校验（`set-token`）

- **仅接受 `stdin`**：移除文件路径参数，支持通过 heredoc（`ghpatd set-token <<'EOF'`）直接注入 ASCII armored 密文，密文无需落盘。
- **强制公钥模式**：仅接受 `age::Decryptor::Recipients`（拒绝 passphrase 加密模式）。
- **解密缓冲清零**：从 `SensitivePage` 读取私钥字节与解密出的明文缓冲均以 `zeroize::Zeroizing` 包裹，离开作用域自动清零。
- **失败原子性**：只有当解密成功且 `GET https://api.github.com/user` 返回 HTTP `200` 时才覆写 `SensitivePage`；任何失败路径（损坏密文、错配公钥、HTTP `401`、网络异常）均**完全保留 daemon 原有状态与旧 PAT 不变**。

### 3.3 凭据取用与瞬态副本收敛（P1-3）

- **内置 `gh` / `api` 子命令**：由 daemon 直接在进程内请求 GitHub REST API，PAT 从不经过 IPC 发送给 `client`。
- **Git `cred-helper` 通道**：
  - 严格域白名单：仅当 `protocol == "https"` 且 `host ∈ {"github.com", "www.github.com"}` 时才返回凭据，其他域名一律拒绝（`HOST_NOT_ALLOWED`）。
  - 用户名固定为 `x-access-token`（兼容 Classic PAT 与 Fine-grained PAT）。
  - daemon 侧 `Outbound::Creds` 不经过 `serde_json` 中间字符串分配，而是直接手工转义拼接进 `Zeroizing<Vec<u8>>`，Socket 写出后立即清零。
  - `cred-helper` 进程写出 `stdout` 后立即将输出缓冲与密码字节数组逐字节覆写为 `0`。

### 3.4 销毁与断开竞态防护（P0-1）

- 收到 `shutdown` IPC 请求、`SIGINT` 或 `SIGTERM` 时，daemon 立即调用 `destroy()`：`SensitivePage::zeroize_all()` → `unlink(ghpatd.sock)` → `exit(0)`。
- **P0-1 竞态保证**：处理 `shutdown` 时，向客户端写回响应为 best-effort；即使客户端在响应写出前提前关闭连接（触发 `BrokenPipe`），daemon 也必定执行 `zeroize_all()` 与 `unlink()`，绝不跳过销毁。

---

## 4. IPC 与文件系统防护

| 防护项 | 机制 |
|---|---|
| **运行目录权限** | 默认 `${XDG_RUNTIME_DIR:-/tmp/ghpatd-$UID}`，自动校验目录归属当前 UID 且权限强制收紧为 `0700`。 |
| **对端身份校验** | 每次 `accept()` 后立即通过 `getsockopt(SO_PEERCRED)` 校验对端 `uid == getuid()`，跨 UID 连接静默断开，不产生任何响应。 |
| **读空闲超时（P1-1）** | 单次连接读取空闲超过 `10s` 自动关闭该连接并记审计 `action=conn_timeout`，防止异常客户端耗尽连接。 |
| **请求行长上限（P1-2）** | 单行请求上限 `64 KiB`（65536 字节）；超长请求立即丢弃余行、返回 `BAD_REQUEST`、关闭连接并记审计 `action=line_too_long`，防止内存耗尽攻击。 |

---

## 5. 审计日志与脱敏规范（N-4）

- **格式**：`[<ISO8601 UTC>] action=<动作> result=<结果> peer_pid=<对端PID或-> [detail=<详情>]`
- **覆盖事件**：`daemon_start`、`token_set`、`get_pat`、`shutdown`、`signal`、`conn_timeout`、`line_too_long`、`conn_error`、`accept`。
- **脱敏指纹**：仅记录形如 `ghp_…9xYz`、`github_pat_…9xYz` 的末尾 4 位指纹，**任何情况下日志中绝不出现 PAT 明文**。
- **容量上限**：日志文件权限 `0600`，超过 `1 MiB` 时自动截断保留后半部分。

---

## 6. 威胁模型矩阵与已知边界

| 攻击面 / 场景 | 是否防御 | 防护机制 / 边界说明 |
|---|---|---|
| **磁盘取证（swap、core dump、临时文件、`~/.git-credentials`）** | **是** | 单页 `mmap + mlock + MADV_DONTDUMP`、`PR_SET_DUMPABLE=0`、`stdin` 注入、`cred-helper store/erase` 空操作 |
| **跨 UID 进程读取 PAT 或注入命令** | **是** | 目录 `0700`、socket `0600`、`SO_PEERCRED` 强制校验 `uid == getuid()` |
| **通过 `ps` / `/proc/<pid>/cmdline` / `environ` 扫描 PAT** | **是** | PAT 不进入任何进程的 `argv` 或环境变量 |
| **无特权同 UID 进程 `ptrace` 附加 daemon** | **是** | `prctl(PR_SET_DUMPABLE, 0)` 阻止无 `CAP_SYS_PTRACE` 进程附加 |
| **恶意/损坏密文或无效 token 破坏现有会话** | **是** | 解密失败或 `GET /user` 401 拒绝注入，原有 `ARMED` 状态与旧 PAT 保持不变 |
| **网络窃听与证书劫持** | **是** | 仅出站 HTTPS，使用内置 `rustls-tls`，仅向 `api.github.com` 与 `github.com` 通信 |
| **同 UID 进程主动调用 `cred-helper get` / `get_pat` 读取 PAT** | **否（授信边界内）** | 同 UID 进程视为可信调用方（允许其读取 PAT、注入命令、结束进程）；应在 Prompt 层约束 AI Agent 不得主动调用底层凭据接口 |
| **宿主 `git` 进程或 `reqwest` HTTP 库内部瞬态缓冲** | **部分** | `ghpatd` 自有缓冲全部 `Zeroizing` 清零；交付给 `git` 进程或第三方 HTTP 库内部的瞬态缓冲依赖其自身内存生命周期 |
| **进程被 `SIGKILL`（`kill -9`）强制杀死** | **依赖内核** | `SIGKILL` 不可捕获，用户态清零钩子无法执行，由 Linux 内核直接回收匿名 `mlock` 物理页；下次 `start` 自动清理残留 socket |
| **具备 `root` 或 `CAP_SYS_PTRACE` / 内核级权限的攻击者** | **否（模型外）** | 超出无 root 用户态守护进程的防御边界 |
