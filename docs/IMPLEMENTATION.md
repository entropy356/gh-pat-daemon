# ghpatd 逐模块实现规格（v0.0.2）

> 复刻级规格之二。[ARCHITECTURE.md](ARCHITECTURE.md) 给出全局架构与协议；本文按模块给出
> 数据结构、函数签名与逐条行为规则。实现语言 Rust 2021（MSRV 1.75），单二进制 crate `gh-pat-daemon`。

## 0. 工程与依赖

`ghpatd/Cargo.toml`（bin 名 `ghpatd`，path `src/main.rs`）：

```toml
[dependencies]
age = "0.10"
tokio = { version = "1", features = ["rt-multi-thread", "net", "io-util", "signal", "process", "macros", "time"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
zeroize = { version = "1", features = ["zeroize_derive"] }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
thiserror = "1"
libc = "0.2"
base64 = "0.22"
dirs = "5"
bech32 = "0.9"
jaq-core = "2"
jaq-std = "2"
jaq-json = { version = "1", features = ["serde_json"] }
jaq-syn = "1"

[profile.release]
lto = "thin"        # 规格建议 fat；沙箱 2 核 + NFS 无法承受，属已记录偏差
strip = true
panic = "unwind"
codegen-units = 1
```

注意：tokio 必须含 `time` feature（读空闲超时，v0.0.2 新增）；reqwest 关闭 default features 只留
rustls-tls + json（musl 静态链接兼容）；bech32 0.9 用于 age 私钥原始字节 ↔ bech32 互转。

模块清单（`mod` 声明于 main.rs）：`page / agekey / daemon / client / gh / cred / wrap / jq / ipc / err`。

---

## 1. `page.rs` —— SensitivePage 与指纹

### 1.1 常量与布局

```rust
const PAGE_SIZE: usize = 4096;
const OFF_IDENTITY: usize = 0;   // [u8; 32]
const OFF_PAT_LEN:  usize = 32;  // u64 LE
const OFF_PAT_BUF:  usize = 40;  // [u8; 256]
pub const PAT_CAP:  usize = 256;
```

### 1.2 `SensitivePage`

```rust
pub struct SensitivePage { page: *mut u8 }   // unsafe impl Send + Sync
```

- `new() -> io::Result<Self>`：mmap(PROT_READ|WRITE, MAP_PRIVATE|MAP_ANONYMOUS) → mlock 整页 →
  madvise(MADV_DONTDUMP)。任一步失败：munmap 后返回 `last_os_error()`。
- 私有 `unsafe write_volatile(off, src)` / `zero_volatile(off, len)`：逐字节 `write_volatile`。
- `set_identity(&self, raw: &[u8; 32])`：一次性写入 OFF_IDENTITY。
- `identity_raw(&self) -> [u8; 32]`：逐字节 read_volatile 读出（瞬态副本，调用方负责 zeroize）。
- `set_pat(&self, pat: &str) -> Result<(), ()>`：len > PAT_CAP → Err；否则
  zero_volatile(OFF_PAT_BUF, max(旧长, 新长)) → zero_volatile(OFF_PAT_LEN, 8) →
  写 pat 字节 → 写 (len as u64).to_le_bytes()。
- `pat(&self) -> Option<&str>`：len==0 或 >PAT_CAP → None；否则
  `from_raw_parts(page+40, len)` + `from_utf8().ok()`（页内借用，不拷贝）。
- `is_armed(&self) -> bool`：pat_len > 0。
- `zeroize_all(&self)`：写零整页 4096。
- `impl Drop`：zeroize_all → munlock → munmap。

### 1.3 `fingerprint(pat: &str) -> String`

```
known = ["ghp_", "github_pat_", "gho_", "ghs_", "ghu_"]
若 pat 以 p 开头且剩余长度 ≥4  → "{p}…{剩余末4字符}"
否则若 pat.len() ≥ 12           → "{前8字符}…{末4字符}"
否则                            → "…"
```

### 1.4 单元测试（2 个）

- `page_set_get_zeroize`：new → 未 armed / pat None → set_pat 覆写两次读回一致 → 257 字节 Err →
  zeroize_all 后 pat None 且 identity 全零。
- `fingerprints`：`ghp_…` 前缀、`github_pat_…` 前缀、未知前缀 `totallyu…9xYz` 三个断言。

---

## 2. `agekey.rs` —— age 密钥管理

```rust
const SECRET_KEY_HRP: &str = "age-secret-key-";
```

- `generate() -> Result<(Identity, [u8; 32], String), String>`：
  `Identity::generate()` → pubkey 字符串（`age1…`）→ `id.to_string()` 得 bech32 SecretString →
  `raw_from_bech32` 解出 32 原始字节 → 返回 (Identity 堆对象, raw, pubkey)。
- `raw_from_bech32(s) -> Option<[u8; 32]>`：`bech32::decode` → 校验 Variant::Bech32 且 hrp 匹配 →
  `Vec<u8>::from_base32` → `try_into`。
- `identity_from_raw(raw: &[u8;32]) -> Result<Identity, String>`：bech32 encode（同 HRP）→
  `to_uppercase()` → `Identity::from_str`。用于从敏感页重建身份。
- `encrypt_for_test(pubkey, plaintext)`（`#[cfg_attr(not(test), allow(dead_code))]`）：
  `Encryptor::with_recipients` → `wrap_output` → write + finish。仅测试用，生产由用户用 age CLI。

### 单元测试（2 个）

- `roundtrip`：generate → pubkey 以 `age1` 开头且长度 ≥20 → identity_from_raw(raw) 的公钥与原一致。
- `encrypt_decrypt_roundtrip`：encrypt_for_test → `Decryptor::new` 应为 Recipients →
  `decrypt(iter::once(&id as &dyn age::Identity))` → 读全文相等。

---

## 3. `ipc.rs` —— IPC 协议数据结构与 client 调用原语

### 3.1 常量与类型

```rust
pub const SOCK_FILE: &str = "ghpatd.sock";
pub const LOG_FILE:  &str = "ghpatd.log";
pub const MAX_LINE:  usize = 64 * 1024;   // P1-2

pub enum Outbound {                        // P1-3
    Plain(Response),
    Creds { id: u64, password: Zeroizing<String> },
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enc_b64: Option<String>,
    pub args: Option<Vec<String>>,   // 同上 skip
    pub repo: Option<String>,
    pub host: Option<String>,
    pub protocol: Option<String>,
}

pub struct ErrorBody { pub code: String, pub message: String }
pub struct Response { pub id: u64, pub ok: bool, pub payload: Option<Value>, pub error: Option<ErrorBody> }
impl Response {
    pub fn ok(id, payload: Value) -> Self
    pub fn err(id, code: &str, message: impl Into<String>) -> Self
}
pub fn next_id() -> u64   // static AtomicU64 从 1 起，fetch_add(Relaxed)
```

### 3.2 路径解析

- `resolve_sock(override: Option<&Path>) -> PathBuf`：`--sock` > `GHPATD_SOCK`（非空）>
  `sock_dir_default().join(SOCK_FILE)`。
- `sock_dir_default()`：`XDG_RUNTIME_DIR`（非空）否则 `/tmp/ghpatd-{getuid()}`。
- `ensure_sock_dir(path) -> Result<(), String>`（§5.3.1）：
  - 已存在：metadata.uid != getuid → Err"目录不属于当前 UID"；mode&0o777 != 0o700 →
    自动 chmod 0700（失败则 Err"收紧权限失败"）。
  - 不存在：mkdir → chmod 0700。

### 3.3 `call(sock, req, timeout) -> Result<Response, io::Error>`

阻塞式（client 形态同步使用）：connect → set_read/write_timeout → `serde_json::to_string` + `'\n'`
→ write_all + flush → BufReader read_line → 空行 → Err(UnexpectedEof,"daemon 关闭连接") →
parse JSON（失败 → InvalidData）。

### 3.4 `probe_connect(sock) -> bool`

`UnixStream::connect(sock).is_ok()`（start 的残留检测用）。

---

## 4. `daemon.rs` —— daemon 形态（核心模块）

### 4.1 状态与常量

```rust
const READ_TIMEOUT: Duration = Duration::from_secs(10);        // P1-1
pub struct Meta { pub pubkey: String, pub login: Option<String>, pub scopes: Vec<String>, pub fingerprint: Option<String>, pub git_user: Option<String>, pub git_email: Option<String> }  // git_user/git_email：start --user-name/--user-email 配置的署名（非敏感）
pub struct DaemonState {
    pub page: Mutex<SensitivePage>,
    pub meta: Mutex<Meta>,
    pub client: reqwest::Client,
    pub sock_path: PathBuf,
}
```

### 4.2 日志

- `log_line(sock_path, msg)`：socket 父目录下 append `LOG_FILE`；打开后若 size > 1MB →
  读全量、`fs::write` 保留后半、重设 0600 再重开；否则 chmod 0600；行前缀
  `[<iso8601_utc(now)>] `。所有 IO 错误静默忽略。
- `log_audit(sock_path, action, result, peer: Option<u32>, detail: Option<&str>)`：
  拼 `action={a} result={r} peer_pid={p|-}`（+ ` detail={d}`）后调 log_line。
- `iso8601_utc(secs: u64) -> String`（civil_from_days，无 chrono）：

```
days = secs/86400; rem = secs%86400; h,mi,s = rem/3600, (rem%3600)/60, rem%60
z = days + 719468
era = z / 146097（负数时 (z-146096)/146097）
doe = z - era*146097                                   // [0,146096]
yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365  // [0,399]
doy = doe - (365*yoe + yoe/4 - yoe/100)                // [0,365]
mp  = (5*doy + 2) / 153                                // [0,11]
d   = doy - (153*mp + 2)/5 + 1                         // [1,31]
mo  = mp<10 ? mp+3 : mp-9
y   = yoe + era*400 + (mo<=2 ? 1 : 0)
格式 "{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z"
```

### 4.3 `run_internal() -> i32`（argv[1] == `--daemon-internal`）

见 [ARCHITECTURE.md](ARCHITECTURE.md) §5.2 十一步序列。要点：

- `GHPATD_SOCK` 缺失 → stderr `ERR missing GHPATD_SOCK`，exit 1。
- `libc::umask(0o077)` → `libc::prctl(PR_SET_DUMPABLE, 0,0,0,0)` → `SensitivePage::new()` →
  `agekey::generate()`（三步任一失败 → stderr `ERR …` exit 1）→ `page.set_identity(&raw)`。
- tokio Runtime → block_on：兜底 remove_file → `UnixListener::bind` → stdout
  `OK {pubkey}\n` + flush → `log_audit(daemon_start, ready)` → reqwest Client(UA `ghpatd`) →
  `Arc<DaemonState>` → signal 任务（SIGTERM/SIGINT select → audit(signal,destroying) → destroy）→
  accept 循环：每连接 `tokio::spawn(handle_conn)`；handle_conn 错误仅
  UnexpectedEof/BrokenPipe/ConnectionReset/WouldBlock 静默，其余 audit(conn_error, io)。
  accept 本身失败 → audit(accept, error)。

### 4.4 连接处理 `handle_conn(stream, state)`

1. `peer_uid_pid`（getsockopt SO_PEERCRED，取 uid/pid）；uid != getuid → 直接 return（静默断开）。
2. `into_split` → `BufReader<OwnedReadHalf>` → loop：
   - `read_line_capped` → Eof: break；Timeout: audit(conn_timeout, closed) + break；
     TooLong: audit(line_too_long, rejected) + 回 `BAD_REQUEST 请求行超长（上限 65536 字节），连接关闭` + break；
   - Line: `serde_json::from_str(line.trim())` 失败 → 回 `BAD_REQUEST 请求解析失败: {e}` + continue；
   - `dispatch` → 若 `req.cmd == "shutdown"` 且响应 ok：best-effort 写回（失败记
     `shutdown/resp_write_failed_destroy_anyway`）→ audit(shutdown, ok) → `destroy(state)`（不返回）；
     非 shutdown 正常写回。

### 4.5 `read_line_capped` / `discard_to_newline`（P1-1 + P1-2）

```
enum LineRead { Line(String), TooLong, Timeout, Eof }
loop {
  avail = timeout(10s, fill_buf)         // Ok(Ok(s)) / Ok(Err(WouldBlock|TimedOut)→Timeout) / Ok(Err(e)→Err) / Elapsed→Timeout
  avail 空 → break（EOF；剩余若有按一行处理）
  本块内找到 '\n' 于 pos：
      bytes.len()+pos > MAX_LINE → consume(pos+1); return TooLong
      否则 append[..pos]; consume(pos+1); break
  bytes.len()+avail.len() > MAX_LINE → consume 整块; discard_to_newline; return TooLong
  append 整块; consume(len)
}
bytes 空 → Eof；否则 Line(from_utf8_lossy)
```

`discard_to_newline`：fill_buf 循环，找到 `\n` → consume(p+1) 结束；块无换行 → consume 整块；
EOF/Err → 结束。

### 4.6 `write_outbound`（P1-3）

```rust
Plain(resp):  Zeroizing(serde_json::to_string(resp) + "\n" → bytes)
Creds { id, password }:
    line = Zeroizing(Vec::with_capacity(96 + password.len()))
    追加 {"id":<id>,"ok":true,"payload":{"username":"x-access-token","password":"
    push_json_escaped(line, password)
    追加 "}}}\n"
write_all + flush
```

`push_json_escaped(buf: &mut Vec<u8>, s: &str)`：逐 char，转义表见 ARCHITECTURE §6；
`c < 0x20` → `format!("\\u{:04x}", c as u32)`；其余 `encode_utf8` 原样。

### 4.7 dispatch 与各命令

```rust
match req.cmd:
  "status"    → cmd_status
  "pubkey"    → cmd_pubkey
  "set_token" → cmd_set_token
  "get_pat"   → cmd_get_pat（返回 Outbound）
  "gh"        → cmd_gh
  "shutdown"  → Response::ok(id, {"ok": true})
  other       → Response::err(id, "BAD_REQUEST", "未知命令: {other}")
```

- **cmd_status**：`{"state": armed? "armed":"ready", "fingerprint": meta.fingerprint}`。
- **cmd_pubkey**：`{"pubkey": meta.pubkey}`。
- **cmd_set_token**（失败一律不改状态）：
  1. `enc_b64` 缺失 → BAD_REQUEST；base64 STANDARD 解码失败 → `BAD_REQUEST base64 解码失败: {e}`。
  2. `strip_age_armor`（见下）。
  3. `age::Decryptor::new`：Recipients → 继续；Passphrase → `NOT_RECIPIENT_FORMAT 仅支持 age -r 公钥加密`；
     Err → `DECRYPT_FAILED 密文与公钥不匹配或已损坏`。
  4. 锁页取 `identity_raw` → `Zeroizing(raw)` → `identity_from_raw` → `decryptor.decrypt(once(&id))`
     失败 → DECRYPT_FAILED 同文案；`read_to_end` 进 Vec → 包 `Zeroizing`（读失败 → DECRYPT_FAILED 密文读取失败）。
  5. `from_utf8` → trim()（容忍管道尾随换行）；空 → DECRYPT_FAILED 明文为空；
     len > PAT_CAP → `PAT_TOO_LONG PAT 超长（>256 字节）`；非 UTF-8 → DECRYPT_FAILED 明文非 UTF-8。
  6. `GET https://api.github.com/user`，头：`Authorization: Bearer {pat}`、
     `Accept: application/vnd.github+json`、`X-GitHub-Api-Version: 2022-11-28`。
     网络失败 → `API_ERROR 访问 GitHub 失败: {e}`。
  7. scopes：响应头 `X-OAuth-Scopes` 有 → `parse_scopes`（逗号分割 trim 去空）；无 →
     `["fine-grained"]`（fine-grained PAT 无此头）。
  8. 401 → `TOKEN_INVALID GitHub 返回 401`；非 200 → `API_ERROR GitHub API {status}: {message}`；
     JSON body 解析失败 → `API_ERROR 响应解析失败: {e}`。
  9. 成功：`page.set_pat(pat_str)`（Err → PAT_TOO_LONG）→ `fingerprint(pat_str)` →
     meta 更新 login/fingerprint/scopes → audit(`token_set, ok, detail=login={login} fp={fp}`) →
     payload `{"login","scopes","fingerprint"}`。
- **cmd_get_pat**：`protocol != "https"` 或 host ∉ {github.com, www.github.com} →
  Plain(err `HOST_NOT_ALLOWED 不为 {host} 代理凭据`)。锁页 `pat()` → None → Plain(err
  `NO_TOKEN PAT 未注入`)；Some → `Zeroizing(p.to_string())`（IPC 传输必需的一次性拷贝）→
  audit(get_pat, ok, caller_pid, detail=host={host}) → `Outbound::Creds { id, password }`。
- **cmd_gh**：`args` 缺失 → BAD_REQUEST；页内 pat → None → NO_TOKEN；Some →
  `p.to_string()`（临时，cmd_gh 边界见 ARCHITECTURE §6）→ `ApiCtx { client, pat: &pat }` →
  `gh::execute(ctx, args, req.repo)` → payload `{"stdout","stderr","exit_code"}`。

### 4.8 `strip_age_armor(bytes) -> Vec<u8>`（N-3）

非 UTF-8 → 原样；trim 后不以 `-----BEGIN AGE ENCRYPTED FILE-----` 开头 → 原样；
否则拼接非空、非 `-----` 开头行 → base64 STANDARD 解码：成功返回明文字节，失败原样返回
（交由 Decryptor 报错）。

### 4.9 单元测试（4 个）

- `destroy_parts_zeroizes_even_when_file_missing`（T-1）：set_pat → destroy_parts → pat None 且
  socket 不存在；对不存在文件重复 destroy_parts 不 panic 且保持清零。
- `age_armor_stripped`：armored → 原字节；非 armored 原样；坏 armor 原样。
- `json_escape_works`：`a"b\c\n\r\t` → `a\"b\\c\n\r\t`；普通串不变。
- `iso8601_known_values`：0→1970-01-01T00:00:00Z；1767225600→2026-01-01；1791018000→2026-10-03T09；
  951782400→2000-02-29（闰年）。

---

## 5. `client.rs` —— client 形态

常量 `IPC_TIMEOUT = 120s`；`call_or_exit`：ipc::call 失败 → stderr
`✘ daemon 未运行，请先执行 ghpatd start` + exit 1。

### 5.1 `start(sock, foreground) -> i32`（见 ARCHITECTURE §5.1）

fork 子进程细节：pipe2(O_CLOEXEC) → fork；子进程 dup2(写端→1)、close 两端、
`GHPATD_SOCK` 合入 env（收集 vars 后追加 set_var）、`Command::new(current_exe).arg("--daemon-internal").exec()`
（exec 失败 → `ERR exec 失败` exit 127）。父进程 close 写端、`File::from_raw_fd(读端)` →
BufReader → 线程 read_line → mpsc → `recv_timeout(5s)`。`OK <pubkey>` → stdout 三行：

```
{pubkey}
daemon 已启动 (pid {child}, socket: {sock})
状态: READY（等待 token 注入）
```

失败/超时 → `✘ daemon 启动失败: {msg}` 或 `✘ daemon 启动超时，exit(1)`。

### 5.2 `set_token(sock)`（N-2/N-3）

stdin read_to_end（失败 → `✘ 读取 stdin 失败`；空 → 用法提示两行，exit 1）→
base64 STANDARD 编码 → IPC set_token。成功打印：

```
✔ 解密成功
✔ 验证通过: {login} (scopes: {join(", ")})
✔ fingerprint: {fp}
状态: ARMED
```

失败按 code：DECRYPT_FAILED / TOKEN_INVALID / NOT_RECIPIENT_FORMAT → 对应文案 + `（当前状态保持不变）`
exit 1；其他 → stderr `✘ {code}: {message}` + 同提示。

### 5.3 stop / status / pubkey / gh

- `stop`：shutdown，10s 超时；ok → `已销毁` exit 0；否则 `✘ daemon 未运行…` exit 1。
- `status`：armed → `状态: ARMED (fingerprint: {fp})`；否则 `状态: READY（等待 token 注入）`。
- `pubkey`：打印 payload.pubkey。
- `gh(sock, args, repo)`：payload.stdout → print!（不换行）、stderr → eprint!、
  返回 exit_code；错误：NO_TOKEN → 固定文案，否则 `✘ {code}: {message}`，exit 1。

### 5.4 `resolve_repo(explicit) -> Result<Option<String>, (Code, String)>`

`--repo` > `GH_REPO` env > `git remote get-url origin`：
- 前两者 `normalize_repo`（去 `https://github.com/` 前缀与 `.git` 后缀）。
- origin：成功 → `parse_remote_url`（https://github.com/{owner}/{repo}[/…] 且段数 ≥2 → Some；
  `git@`/`ssh://` → None → Err(REMOTE_NOT_HTTPS, url)）；git 失败 → Ok(None)（交由 daemon 报错）。

---

## 6. `gh.rs` —— daemon 内 gh 子命令 REST 实现

### 6.1 基础设施

```rust
pub struct GhResult { pub stdout: String, pub stderr: String, pub exit_code: i32 }  // ok()/fail() 构造
pub struct ApiCtx<'a> { pub client: &'a reqwest::Client, pub pat: &'a str }
const API_BASE: &str = "https://api.github.com";
```

- `headers(pat)`：Authorization Bearer、Accept `application/vnd.github+json`、UA `ghpatd`、
  `X-GitHub-Api-Version: 2022-11-28`。
- `api_call(ctx, method, url, body) -> Result<(u16, Value), String>`：method ∈ GET/POST/PUT/PATCH/DELETE
  （否则 Err"不支持的 HTTP 方法"）；body → `.json(b)`；响应 text → 空则 Value::Null，
  否则 parse（失败 → Value::String(text)）。
- `api_list(ctx, endpoint, query, limit) -> Result<Vec<Value>, String>`：`per_page=min(limit,100)` →
  追加 `page=N` 翻页直到 items≥limit 或返回条数 < per_page → `truncate(limit)`。
- `api_err(status, val)`：`GitHub API {status}: {message|"（无 message）"}`（401 同格式）。
- 表格：`display_width`（`c as u32 > 0x2E7F` 计 2 列，CJK 宽字符）+ `pad` + `render_table`（两空格分隔，
  行尾 trim）。

### 6.2 `execute(ctx, args, repo)`（分发）

`args[0]` ∈ api / auth / repo / pr / issue，else `Err("未知子命令")`；Err 包装为
`GhResult::fail("✘ {msg}\n", 1)`（401 与其他错误均 exit 1；401 不清 PAT）。

### 6.3 子命令

- **api**（`cmd_api`）：手写参数解析：`--method|-X`、`--field|-f`（k=v，格式错 →
  Err"--field 格式应为 k=v"）、`--jq`；endpoint 去前导 `/`。GET/DELETE 或无 fields → fields 拼查询串；
  否则 fields 组成 JSON body（值能 parse 成 JSON 则用之，否则按字符串）。非 2xx → Err(api_err)。
  `--jq` → `apply_jq`（结果包数组）→ pretty 输出；无 → pretty 原值。
- **auth status**：GET /user；非 200 → Err(api_err)；成功 → `已认证为 {login}\n`。
- **repo view**：目标 = 位置参数 or repo（缺 → Err）；GET /repos/{target}；固定 7 行输出
  （名称/描述/可见性/默认分支/Stars/更新时间/URL，缺失 `-`）。
- **repo list**：`--limit`（默认 30）；GET /user/repos?sort=updated（api_list）；表格列
  NAME/DESCRIPTION/UPDATED。
- **pr list**：`common_list_opts`（`--state`（默认 open）/`--limit|-L`/`--json`/`--jq`）；
  GET /repos/{t}/pulls?state=…；`--json` → `json_output`；否则表格 NUMBER/TITLE/BRANCH/STATE
  （`pr_row`：`#{number}`、title、head.ref、state 大写）。
- **pr view**：GET /repos/{t}/pulls/{n}；输出 title/state/author/branch(`head→base`)/url 五行。
- **pr create**：`--title|-t --head|-H --base|-B --body|-b`（前三必填）→
  POST /repos/{t}/pulls → `已创建 PR #{n}: {html_url}\n`。
- **pr merge**：`--merge`（默认）/`--squash|-s`/`--rebase|-r` → PUT /repos/{t}/pulls/{n}/merge
  body `{"merge_method": m}` → `已合并 PR #{n}（{m}）: {message}\n`。
- **issue list**：同 pr list 但 GET /repos/{t}/issues，且 `retain(!has pull_request)`（issues API
  含 PR，与 gh 行为一致）；表格 NUMBER/TITLE/STATE。
- **issue view**：GET /repos/{t}/issues/{n}；title/state/author 三行。
- **issue create**：`--title|-t --body|-b`（title 必填）→ POST /repos/{t}/issues →
  `已创建 Issue #{n}: {html_url}\n`。

### 6.4 `--json` 字段集与映射

支持字段（JSON_FIELDS）：`number, title, state, headRefName, baseRefName, url, author, createdAt, isDraft`。
未知字段 → Err（提示 v0.0.1 支持: 列表）。REST 映射（map_json_field）：

```
number→number, title→title, state→state, headRefName→head.ref, baseRefName→base.ref,
url→html_url, author→user.login, createdAt→created_at, isDraft→draft
```

输出规则（json_output）：`--json` 单独 → pretty 数组；`--json`+`--jq` → 先投影再 jaq → pretty；
仅 `--jq` → Err"--jq 需与 --json 配合使用"。

---

## 7. `cred.rs` —— cred-helper 形态

`run(argv) -> i32`：

1. op 解析：argv[1]=="cred-helper" 则取 argv[2]；`get` 继续，`store`/`erase` → 静默 exit 0
   （不落盘任何凭据），其他 exit 1。
2. stdin 逐行读至空行，仅取 `protocol=`、`host=`。
3. 域过滤：`protocol != "https"` 或 host ∉ {github.com, www.github.com} → exit 0（空输出）。
4. IPC get_pat（10s 超时；非 ok / daemon 不在 → exit 0，git 将走 prompt/失败）。
5. 输出 `username=x-access-token\npassword={pat}\n\n`；写出后逐字节清零 stdout 缓冲与
   password 的 Vec。

## 8. `wrap.rs` —— wrap 形态

`run(sock, command) -> i32`：command 空 → `✘ wrap 需要 -- 后跟目标命令` exit 1。
`current_exe` → helper 值 `!{exe} cred-helper`。先经 IPC `getuser`（5s 超时，失败按未配置处理）
查询署名，env 注入（嵌套 wrap 兼容：从既有 `GIT_CONFIG_COUNT` 读取偏移 idx，写回 idx+1+N）：

```
GIT_CONFIG_COUNT = existing + 1 + N        # N = 署名条目数（0/1/2）
GIT_CONFIG_KEY_{idx+1} = user.name         # 配置了 --user-name 时
GIT_CONFIG_KEY_{idx+2} = user.email        # 配置了 --user-email 时
GIT_CONFIG_KEY_{idx}   = credential.https://github.com.helper
GIT_CONFIG_VALUE_{idx} = !{exe} cred-helper
GIT_TERMINAL_PROMPT    = 0
GHPATD_SOCK             = {sock}
```

`Command::new(command[0]).args(rest).status()`；退出码透传（None → 1）；启动失败 →
`✘ 启动目标命令失败: {e}` exit 127。

## 9. `jq.rs` —— --jq 支持（jaq 内存执行）

`apply(filter_src, input: &Value) -> Result<Vec<Value>, String>`：
`File { code, path: () }` → `Loader::new(jaq_std::defs().chain(jaq_json::defs()))` → load（Err →
"jq 语法错误: …"）→ `Compiler::default().with_funs(jaq_std::funs().chain(jaq_json::funs()))` →
compile（Err → "jq 编译错误: …"）→ `RcIter::new(empty())` 输入、`filter.run((Ctx::new([], …), Val::from(input.clone())))`
→ 收集全部输出（Err → "jq 运行错误: …"）。

## 10. `err.rs` —— 错误码与 client 文案

```rust
pub enum Code { DaemonNotRunning, SocketStale, AlreadyRunning, NoToken, DecryptFailed,
  TokenInvalid, TokenExpired, ApiError, NotRecipientFormat, RemoteNotHttps, UidMismatch,
  StartTimeout, HostNotAllowed, PatTooLong, Io }
Code::as_str() → "DAEMON_NOT_RUNNING" / "SOCKET_STALE" / "DAEMON_ALREADY_RUNNING" / "NO_TOKEN" /
  "DECRYPT_FAILED" / "TOKEN_INVALID" / "TOKEN_EXPIRED" / "API_ERROR" / "NOT_RECIPIENT_FORMAT" /
  "REMOTE_NOT_HTTPS" / "UID_MISMATCH" / "START_TIMEOUT" / "HOST_NOT_ALLOWED" / "PAT_TOO_LONG" / "IO_ERROR"
impl Display（as_str）
```

`client_message(code, detail)` 固定文案（其余 code → `✘ {code}{: detail}`）：

```
DaemonNotRunning   ✘ daemon 未运行，请先执行 ghpatd start
SocketStale        ✘ 残留 socket 已自动清理，请重试
AlreadyRunning     ✘ daemon 已在运行（DAEMON_ALREADY_RUNNING）
NoToken            ✘ PAT 未注入，请执行 ghpatd set-token（stdin）
DecryptFailed      ✘ 解密失败：密文与公钥不匹配或已损坏
TokenInvalid       ✘ token 无效（401），注入被拒绝；保留原状态{：detail}
NotRecipientFormat ✘ 仅支持 age -r 公钥加密（NOT_RECIPIENT_FORMAT）
RemoteNotHttps     ✘ ghpatd 仅支持 HTTPS remote
StartTimeout       ✘ daemon 启动超时，exit(1)
```

## 11. `main.rs` —— CLI 面与形态分叉

argv 分叉（先于 clap）：argv[1]=="cred-helper" → `cred::run(&argv[1..])` exit；
argv[1]=="--daemon-internal" → `daemon::run_internal()` exit。否则 clap 解析：

- 全局 `--sock <PATH>`（测试隔离）。
- 子命令与参数：

```
start [--foreground]
set-token                     # 仅 stdin（N-2）
stop | status | pubkey
wrap -- <command>…            # last = true
api <endpoint> [--method GET] [--field KEY=VALUE]… [--jq EXPR]
repo view [REPO] [-R/--repo REPO]
repo list [--limit 30] [-R]
pr list  [--state open] [--limit 30] [-R] [--json] [--jq]
pr view  <number> [-R] [--json] [--jq]
pr create --title --head --base [--body] [-R]
pr merge  <number> [--merge|--squash|--rebase] [-R]
issue list  [--state open] [--limit 30] [-R] [--json] [--jq]
issue view  <number> [-R] [--json] [--jq]
issue create --title [--body] [-R]
auth status
```

repo/pr/issue 的 `-R` 统一走 `client::resolve_repo`（Err → stderr client_message exit 1），
再折叠为 gh args（`["pr","list","--state",…,"--limit",…(+ --json/--jq)]` 等）调用 `client::gh`；
`api` 经 `build_api_args` 组 `["api", endpoint, "--method", m, (--field k=v)…, (--jq q)?]`。
进程退出码 = 对应 client 函数返回值。
