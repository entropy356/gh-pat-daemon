# ghpatd 架构与协议规格（v0.0.2）

> 本文档为复刻级规格之一，配套文档：
> [IMPLEMENTATION.md](IMPLEMENTATION.md)（逐模块实现规格）、[REPLICATION.md](REPLICATION.md)（复刻步骤与验收标准）。
> 三份文档合起来可在不参考源码的前提下完整复刻全部代码（11 个 Rust 模块，约 2800 行）。

## 0. 项目定位

`ghpatd` 是面向 AI 智能体的 GitHub PAT 内存代理：单二进制（Rust，x86_64 Linux，musl 静态链接），
PAT 全生命周期不落盘、不进 argv/env，仅存于 daemon 进程的 mlock 内存页。三种形态由 argv 分叉：

| 形态 | 入口 argv[1] | 职责 |
|---|---|---|
| client | 子命令（start/set-token/stop/status/pubkey/wrap/api/repo/pr/issue/auth） | 用户/智能体入口，经 IPC 与 daemon 通信 |
| daemon | `--daemon-internal` | 常驻，持有敏感页，处理 IPC 请求，执行 GitHub REST |
| cred-helper | `cred-helper` | git credential helper 协议适配 |

命名约定：项目 `gh-pat-daemon`，二进制 `ghpatd`，socket `ghpatd.sock`，日志 `ghpatd.log`。

## 1. 总体架构

```
                     ┌────────────────────────────────────────────┐
  用户/智能体 ──argv──▶ client 形态                                  │
                     │  start/set-token/stop/status/pubkey/…      │
                     └───────┬────────────────────────────────────┘
                             │ Unix socket（JSON Lines 一问一答）
                             ▼
  age 公钥 ◀── start 输出 ┌────────────────────────────────────────────┐
  age 密文 ──stdin──▶ set-token│ daemon 形态（--daemon-internal）          │
                             │  tokio 事件循环                             │
                             │  DaemonState {                             │
                             │    SensitivePage（mlock 4KiB）             │
                             │    Meta { pubkey, login, scopes, fp }      │
                             │    reqwest::Client                         │
                             │    sock_path }                             │
                             └───────┬────────────────────────────────────┘
                                     │ Bearer PAT
                                     ▼
                               api.github.com

  git ──credential helper 协议──▶ cred-helper 形态 ──IPC get_pat──▶ daemon
  wrap：为目标命令注入 GIT_CONFIG_* / GIT_TERMINAL_PROMPT=0 / GHPATD_SOCK 后 exec；配置了署名时额外注入 user.name/user.email 条目（经 getuser 查询）
```

数据流要点：

1. **注入链**：daemon 启动时进程内生成 age 密钥对 → 私钥 32 原始字节写入敏感页，公钥打印给用户 →
   用户在本机用 `age -r <pubkey>` 加密 PAT → 密文经 stdin（client）→ base64（IPC）→ daemon 解密 →
   `GET /user` 验证 → 写入敏感页。
2. **取用链**：cred-helper / wrap 经 IPC `get_pat` 取回 PAT；`api/repo/pr/issue/auth` 子命令
   把 gh 风格参数透传给 daemon，由 daemon 内部执行 REST 调用（PAT 不出 daemon）。
3. **销毁链**：`stop`/`shutdown`/SIGINT/SIGTERM → volatile 写零整页 → unlink socket → exit(0)。

## 2. 文件与目录布局

| 路径 | 说明 | 权限 |
|---|---|---|
| socket 路径 | `--sock` > 环境变量 `GHPATD_SOCK` > `${XDG_RUNTIME_DIR:-/tmp/ghpatd-$UID}/ghpatd.sock` | 0600（umask 077 先行） |
| sock 父目录 | 回退 /tmp 时：归属当前 UID 且 0700（已存在但权限过宽时自动收紧） | 0700 |
| 日志 | socket 同目录 `ghpatd.log` | 0600，超 1MB 截断保留后半 |

## 3. IPC 协议（JSON Lines over Unix socket）

一连接可多请求（client 均为单请求一连接）；每请求/响应各占一行，以 `\n` 结尾。

### 3.1 请求

```json
{"id":1,"cmd":"<命令>","enc_b64":"…?","args":["…"?],"repo":"…?","host":"…?","protocol":"…?"}
```

- 字段除 `id`、`cmd` 外均可选（serde `skip_serializing_if = Option::is_none`）。
- `id`：client 侧进程级 AtomicU64 自增（初始 1）。

命令集与必填字段：

| cmd | 必填 | 语义 |
|---|---|---|
| `status` | — | 返回 `{state: "ready"/"armed", fingerprint}` |
| `pubkey` | — | 返回 `{pubkey}`（与规格的偏差：v0.0.2 新增） |
| `getuser` | — | 返回 `{user, email}`（start --user-name/--user-email 配置的 git 署名；未配置为 null） |
| `set_token` | `enc_b64` | base64(age 密文) → 验证 → 写页 |
| `get_pat` | `host`、`protocol` | 仅供 cred-helper；域过滤后返回凭据响应 |
| `gh` | `args`（可含 `repo`） | daemon 内执行 gh 子命令语义的 REST |
| `shutdown` | — | 销毁（见 §5.4 竞态语义） |
| 其他 | — | `BAD_REQUEST: 未知命令` |

### 3.2 响应（普通）

```json
{"id":1,"ok":true,"payload":{…}}
{"id":1,"ok":false,"error":{"code":"DECRYPT_FAILED","message":"…"}}
```

### 3.3 响应（凭据类，`get_pat` 成功时专用）

不走 serde，手工拼接（P1-3，见 §6）：

```json
{"id":<id>,"ok":true,"payload":{"username":"x-access-token","password":"<PAT>"}}
```

username 固定 `x-access-token`（fine-grained PAT 的 git 认证用户名）。

### 3.4 连接级防护（v0.0.2）

- **UID 校验**：accept 后 `SO_PEERCRED` 校验对端 uid == daemon uid，不符静默断开（无响应）。
- **读空闲超时 10s**（P1-1）：`tokio::time::timeout` 包裹每次 `fill_buf`；超时只关当前连接，记审计
  `action=conn_timeout result=closed`。tokio 的 `UnixStream` 无 `set_read_timeout`，故以超时实现。
- **行长上限 64KB**（P1-2）：`MAX_LINE = 64 * 1024`。超限：丢弃至行尾（含半行残数据）、
  回 `BAD_REQUEST`、关连接、记审计 `action=line_too_long result=rejected`。
  合法请求最大者为 set_token 的 base64 密文，裕量巨大。

## 4. 敏感页 SensitivePage（内存模型）

单页 4KiB：`mmap(MAP_PRIVATE|MAP_ANONYMOUS)` → `mlock` 整页 → `madvise(MADV_DONTDUMP)`。
进程生命周期内不 munlock、不 realloc。`unsafe impl Send + Sync`（跨线程经 `Arc<Mutex<..>>`）。

页内布局（固定偏移）：

| 偏移 | 内容 |
|---|---|
| 0..32 | age x25519 私钥原始 32 字节 |
| 32..40 | pat_len（u64 LE） |
| 40..296 | pat_buf（`[u8; 256]`，`PAT_CAP = 256`） |
| 296..4096 | 预留（保持零填充） |

访问规则：

- 所有读写用 `write_volatile`/`read_volatile` 逐字节进行（防编译器优化掉敏感写）。
- `set_pat`：先 zeroize 旧内容区（`max(旧长, 新长)`）与 pat_len，再写新值（原地覆写，§5.1）。
- `pat()`：返回页内 `&str` 借用，禁止克隆到页外（调用方如需拷贝，须用 `Zeroizing` 包裹）。
- `zeroize_all`：volatile 写零整页；`Drop`：zeroize_all → munlock → munmap。
- `is_armed`：pat_len > 0。

## 5. 生命周期

### 5.1 start（client 侧 fork 流程）

1. `ensure_sock_dir`：校验/创建父目录（§2）。
2. 残留检测：socket 文件存在 → 试连接；可连 → `DAEMON_ALREADY_RUNNING` exit 1；
   不可连 → 视为残留，unlink。
3. `pipe2(O_CLOEXEC)` 匿名管道 → `fork()`：
   - 子进程：写端 dup2 到 stdout、关读端，设 `GHPATD_SOCK` env，`exec` 自身 `--daemon-internal`。
   - 父进程：关写端，起线程阻塞读第一行，5s 超时。
4. 收到 `OK <pubkey>` → 打印公钥/pid/socket/READY，exit 0；`ERR …` 或超时 → `START_TIMEOUT` exit 1。

`start --foreground`：不 fork，当前进程 `set_var("GHPATD_SOCK")` 后直接进入 daemon 模式。

### 5.2 daemon 启动序列（次序有讲究）

1. 读 `GHPATD_SOCK` env（缺失 → stderr `ERR missing GHPATD_SOCK`，exit 1）。
2. `umask(0o077)` —— 先于 bind，socket 文件创建即 0600（消除 bind→chmod 的 TOCTOU）。
3. `prctl(PR_SET_DUMPABLE, 0)` —— 在创建任何敏感数据之前禁 core dump。
4. 创建 SensitivePage（失败 → `ERR mmap/mlock 失败` exit 1）。
5. 生成 age 密钥对；原始 32 字节写页（`set_identity`）；保留堆上 `Identity` 对象用于解密
   （其内部 Secret drop 时自行 zeroize）。
6. 建 tokio multi-thread runtime。
7. 兜底 `remove_file(sock)` → `UnixListener::bind`。
8. 向 stdout（管道写端）写 `OK <pubkey>\n` 并 flush。
9. 审计 `action=daemon_start result=ready`；构建 `reqwest::Client`（UA: `ghpatd`）与 `DaemonState`。
10. 注册 SIGINT/SIGTERM handler（tokio::select 任一触发 → 审计 `action=signal result=destroying` → destroy）。
11. accept 循环：每连接 spawn 任务（§3.4 防护 + §3.1 dispatch）。

### 5.3 运行中状态

- `ready`：页内无 PAT；`armed`：已注入。指纹格式见 §7。
- `set_token` 全流程见 [IMPLEMENTATION.md](IMPLEMENTATION.md) §daemon；任何失败路径不改动现有
  页内状态（"当前状态保持不变"）。
- 401 不自动清零（保留现场便于诊断，规格 §9 决策）。

### 5.4 销毁语义与竞态修复（P0-1）

`destroy` = `destroy_parts`（锁页 → `zeroize_all` → `remove_file(sock)`，两步错误均忽略）
→ `process::exit(0)`。

`shutdown` 处理的关键规则：**响应写回是 best-effort，无论写回成败必执行 destroy**。
（v0.0.1 缺陷：客户端在响应写回前断开 → BrokenPipe → daemon 跳过销毁，socket 残留、敏感页未清。
回归用例：发送 shutdown 后立即 close，4s 内 socket 必须消失、进程必须退出。）

## 6. 敏感数据处理约定

| 数据 | 存放 | 拷贝策略 |
|---|---|---|
| age 私钥 | 敏感页 0..32（唯一权威） | 解密时经 `identity_raw()` 重建 Identity，瞬态副本 `Zeroizing` |
| PAT（静态） | 敏感页 40..296 | 页外副本必须 `Zeroizing`；写出后清零 |
| PAT（IPC 出口） | `Outbound::Creds { password: Zeroizing<String> }` | 手工 JSON 拼接进 `Zeroizing<Vec<u8>>`，`write_all` 后 drop 即清零 |
| PAT（client stdin→IPC） | stdin 字节 → base64 字符串（密文，非敏感） | 明文只在 daemon 解密后短暂存在于 `Zeroizing` 缓冲 |
| cred-helper stdout 缓冲 | 含 PAT | 写出后逐字节清零（含 password 变量） |
| `cmd_gh` 请求头 | reqwest 内部 | **已知边界**：第三方库内部缓冲无法保证清零（文档如实标注） |

JSON 手工拼接转义表（`push_json_escaped`）：`"`→`\"`、`\`→`\\`、`\n`→`\\n`、`\r`→`\\r`、
`\t`→`\\t`、其他 <0x20 → `\u00XX`；其余按 UTF-8 原样。**禁止中途生成中间 String 副本。**

## 7. 指纹与审计

### 7.1 指纹（`fingerprint`）

识别前缀 `ghp_ / github_pat_ / gho_ / ghs_ / ghu_`：输出 `<前缀>…<剩余末 4 字符>`；
未知前缀且总长 ≥12：`<前 8 字符>…<末 4 字符>`；否则 `…`。

### 7.2 审计日志（N-4）

格式：`[<ISO8601 UTC>] action=<动作> result=<结果> peer_pid=<对端或-> [detail=<...>]`

- ISO8601 由 Unix 秒换算（civil_from_days 算法，无 chrono），格式 `YYYY-MM-DDTHH:MM:SSZ`。
- 覆盖事件：`daemon_start/ready`、`token_set/ok`（detail=`login=<login> fp=<指纹>`，仅指纹）、
  `get_pat/ok`（detail=`host=…`）、`shutdown/ok`、`shutdown/resp_write_failed_destroy_anyway`、
  `signal/destroying`、`conn_timeout/closed`、`line_too_long/rejected`、`conn_error`、`accept/error`。
- **硬性约束：日志任何位置不得出现 PAT 明文。**
- 文件 0600；>1MB 时读全量、保留后半覆写、重设 0600。

## 8. 威胁模型（规格 §8，v0.0.2 不变）

1. 同 UID 进程可取回 PAT（IPC 不认证调用方身份、`/proc/<pid>/mem` 防护超范围）——模型内边界。
2. shutdown 无认证（同 UID 本可 get_pat，单独保护收益有限）。
3. get_pat 返回真实 PAT，凭据进入 git/目标命令进程内存，依赖宿主进程纪律。
4. daemon 启动无父进程校验（同 1）。
5. v0.0.2 的 P0-1/P1-1/P1-2/P1-3 均为模型内收敛：竞态销毁、连接资源、内存耗尽、堆上瞬态副本。

## 9. 错误码（详见 err.rs）

`DAEMON_NOT_RUNNING / SOCKET_STALE / DAEMON_ALREADY_RUNNING / NO_TOKEN / DECRYPT_FAILED /
TOKEN_INVALID / TOKEN_EXPIRED / API_ERROR / NOT_RECIPIENT_FORMAT / REMOTE_NOT_HTTPS /
UID_MISMATCH / START_TIMEOUT / HOST_NOT_ALLOWED / PAT_TOO_LONG / IO_ERROR`。

client 侧固定文案（err::client_message）与"当前状态保持不变"提示规则见
[IMPLEMENTATION.md](IMPLEMENTATION.md) §err。
