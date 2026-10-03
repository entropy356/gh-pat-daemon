//! daemon 形态（规格 §5、§6、§8）
//!
//! v0.0.2 变更：
//! - P0-1：shutdown/stop 销毁不再依赖响应写回结果（竞态修复）
//! - P1-1：连接读空闲超时 10s，异常客户端不再长期占用
//! - P1-2：请求行长上限 64KB，超长拒绝并关闭连接、记审计
//! - P1-3：get_pat 响应序列化走手工拼接 + Zeroizing 缓冲（自有缓冲清零）
//! - N-3：set_token 兼容 ASCII armored 的 age 密文
//! - N-4：审计日志统一格式（ISO8601 UTC + action/result/peer_pid）

use crate::err::Code;
use crate::gh::{ApiCtx, GhResult};
use crate::ipc::{Outbound, Request, Response, LOG_FILE, MAX_LINE};
use crate::page::{fingerprint, SensitivePage};
use serde_json::json;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use zeroize::Zeroizing;

/// v0.0.2 P1-1：连接读空闲超时。正常命令（含 set_token 的 /user RTT）远小于此值；
/// 超时按"空闲"处理——只关当前连接，不影响 daemon。
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Meta {
    pub pubkey: String,
    pub login: Option<String>,
    pub scopes: Vec<String>,
    pub fingerprint: Option<String>,
    /// git 提交署名（start --user-name/--user-email，可选；非敏感数据）
    pub git_user: Option<String>,
    pub git_email: Option<String>,
}

pub struct DaemonState {
    pub page: Mutex<SensitivePage>,
    pub meta: Mutex<Meta>,
    pub client: reqwest::Client,
    pub sock_path: PathBuf,
}

/// 日志（§7.2）：仅错误与状态变更；超 1MB 截断保留后半
pub fn log_line(sock_path: &Path, msg: &str) {
    let Some(dir) = sock_path.parent() else { return };
    let path = dir.join(LOG_FILE);
    let _ = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|f| {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = f.metadata() {
                if md.size() > 1024 * 1024 {
                    drop(f);
                    // 截断保留后半
                    if let Ok(all) = std::fs::read(&path) {
                        let keep = &all[all.len() / 2..];
                        let _ = std::fs::write(&path, keep);
                    }
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    return std::fs::File::options().create(true).append(true).open(&path);
                }
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
            Ok(f)
        })
        .and_then(|mut f| {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            writeln!(f, "[{}] {}", iso8601_utc(ts), msg)
        });
}

/// 审计行（N-4）：统一 action/result/peer_pid 字段，安全事件全覆盖；不得包含 PAT 明文
pub fn log_audit(sock_path: &Path, action: &str, result: &str, peer: Option<u32>, detail: Option<&str>) {
    let peer_s = peer.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
    let mut msg = format!("action={action} result={result} peer_pid={peer_s}");
    if let Some(d) = detail {
        msg.push_str(&format!(" detail={d}"));
    }
    log_line(sock_path, &msg);
}

/// Unix 秒 → ISO8601 UTC（N-4）；无 chrono 依赖，civil_from_days 算法
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = yoe + era * 400 + if mo <= 2 { 1 } else { 0 };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// daemon 子进程入口（argv[1] == "--daemon-internal"，§5.3.4）
/// 返回退出码；stdout 为与父进程通信的管道（OK <公钥> / ERR <原因>）
pub fn run_internal() -> i32 {
    let sock_path: PathBuf = match std::env::var("GHPATD_SOCK") {
        Ok(s) if !s.is_empty() => s.into(),
        _ => {
            eprintln!("ERR missing GHPATD_SOCK");
            return 1;
        }
    };

    // a. umask 先行，bind 创建即 0600（消除 bind→chmod TOCTOU）
    unsafe { libc::umask(0o077) };

    // c/d. 关 core dump + 分配敏感页（在 bind 前完成敏感初始化次序无影响，
    // 但 PR_SET_DUMPABLE 要在创建任何敏感数据前设置）
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };

    let page = match SensitivePage::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERR mmap/mlock 失败: {e}");
            return 1;
        }
    };

    // 生成 age 密钥对；私钥原始字节写入 SensitivePage
    let (_identity, raw_identity, pubkey0) = match crate::agekey::generate() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ERR identity 生成失败: {e}");
            return 1;
        }
    };
    page.set_identity(&raw_identity);
    let pubkey = pubkey0;

    // b. tokio 运行时
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ERR tokio 初始化失败: {e}");
            return 1;
        }
    };

    rt.block_on(async move {
        // e. bind + listen
        // 先清理残留 socket 文件（父进程已做过一次，此处兜底）
        let _ = std::fs::remove_file(&sock_path);
        let listener = match UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ERR bind 失败: {e}");
                return 1;
            }
        };

        // f. 就绪消息
        let mut out = std::io::stdout();
        let _ = writeln!(out, "OK {pubkey}");
        let _ = out.flush();

        // 后台日志重定向：daemon 后续错误写 ghpatd.log
        log_audit(&sock_path, "daemon_start", "ready", None, None);

        let client = reqwest::Client::builder()
            .user_agent("ghpatd")
            .build()
            .expect("reqwest client");
        let state = Arc::new(DaemonState {
            page: Mutex::new(page),
            meta: Mutex::new(Meta {
                pubkey,
                login: None,
                scopes: Vec::new(),
                fingerprint: None,
                git_user: std::env::var("GHPATD_USER_NAME").ok().filter(|s| !s.is_empty()),
                git_email: std::env::var("GHPATD_USER_EMAIL").ok().filter(|s| !s.is_empty()),
            }),
            client,
            sock_path: sock_path.clone(),
        });

        // 信号处理：SIGINT/SIGTERM → 销毁退出（§5.4）
        {
            let state = state.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                let mut term = signal(SignalKind::terminate()).expect("sigterm");
                let mut int = signal(SignalKind::interrupt()).expect("sigint");
                tokio::select! {
                    _ = term.recv() => {},
                    _ = int.recv() => {},
                }
                log_audit(&state.sock_path, "signal", "destroying", None, None);
                destroy(&state);
            });
        }

        // g. 事件循环
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(stream, state.clone()).await {
                            match e.kind() {
                                std::io::ErrorKind::UnexpectedEof
                                | std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::WouldBlock => {}
                                _ => {
                                    log_audit(&state.sock_path, "conn_error", "io", None, Some(&e.to_string()))
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    log_audit(&sock_path, "accept", "error", None, Some(&e.to_string()));
                }
            }
        }
    })
}

/// 销毁的实质步骤（§5.4）：zeroize 整页 → unlink socket。
/// v0.0.2 T-1：拆出以便单测覆盖"unlink 失败也必须完成 zeroize"的错误分支。
fn destroy_parts(page: &Mutex<SensitivePage>, sock_path: &Path) {
    if let Ok(page) = page.lock() {
        page.zeroize_all();
    }
    // 错误分支：文件不存在 / 已被删也继续，不影响销毁语义
    let _ = std::fs::remove_file(sock_path);
}

/// 销毁流程（§5.4）：zeroize 整页 → unlink socket → exit(0)
fn destroy(state: &DaemonState) -> ! {
    destroy_parts(&state.page, &state.sock_path);
    std::process::exit(0);
}

/// SO_PEERCRED 校验 uid 并取调用方 PID（§8.1）
fn peer_uid_pid(stream: &tokio::net::UnixStream) -> Option<(u32, u32)> {
    unsafe {
        let mut ucred: libc::ucred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let ret = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if ret != 0 {
            return None;
        }
        Some((ucred.uid, ucred.pid as u32))
    }
}

async fn handle_conn(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> std::io::Result<()> {
    // §8.1：accept 后校验 uid == daemon uid
    if peer_uid_pid(&stream).map(|(uid, _)| uid) != Some(unsafe { libc::getuid() }) {
        return Ok(()); // UID_MISMATCH：静默断开（连接不产生任何响应）
    }

    let peer_pid = peer_uid_pid(&stream).map(|(_, pid)| pid);
    // P1-1：读空闲超时在 read_line_capped 内以 tokio::time::timeout 实现
    // （tokio 的 UnixStream 不提供 set_read_timeout）
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader); // tokio::io::BufReader

    loop {
        match read_line_capped(&mut lines).await? {
            LineRead::Eof => break,
            LineRead::Timeout => {
                log_audit(&state.sock_path, "conn_timeout", "closed", peer_pid, None);
                break;
            }
            LineRead::TooLong => {
                // P1-2：超长拒绝并关闭连接，记审计
                log_audit(&state.sock_path, "line_too_long", "rejected", peer_pid, None);
                let resp = Outbound::Plain(Response::err(
                    0,
                    "BAD_REQUEST",
                    format!("请求行超长（上限 {MAX_LINE} 字节），连接关闭"),
                ));
                let _ = write_outbound(&mut writer, &resp).await;
                break;
            }
            LineRead::Line(line) => {
                let req: Request = match serde_json::from_str(line.trim()) {
                    Ok(r) => r,
                    Err(e) => {
                        let resp = Outbound::Plain(Response::err(0, "BAD_REQUEST", format!("请求解析失败: {e}")));
                        write_outbound(&mut writer, &resp).await?;
                        continue;
                    }
                };
                let resp = dispatch(&state, &req, peer_pid).await;
                let shutdown = req.cmd == "shutdown" && matches!(&resp, Outbound::Plain(r) if r.ok);
                if shutdown {
                    // P0-1：响应写回为 best-effort；客户端在写回前断开也必须销毁
                    if write_outbound(&mut writer, &resp).await.is_err() {
                        log_audit(&state.sock_path, "shutdown", "resp_write_failed_destroy_anyway", peer_pid, None);
                    }
                    log_audit(&state.sock_path, "shutdown", "ok", peer_pid, None);
                    destroy(&state);
                }
                write_outbound(&mut writer, &resp).await?;
            }
        }
    }
    Ok(())
}

enum LineRead {
    Line(String),
    TooLong,
    Timeout,
    Eof,
}

/// P1-2：带行长上限的读一行（'\n' 结尾）。超限时丢弃整行（含未到达的尾部）
/// 并返回 TooLong，保证连接内不残留半行数据。
async fn read_line_capped(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> std::io::Result<LineRead> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        // P1-1：空闲超时包裹每次缓冲读取
        let avail = match tokio::time::timeout(READ_TIMEOUT, reader.fill_buf()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Ok(LineRead::Timeout);
            }
            Ok(Err(e)) => return Err(e),
            Err(_elapsed) => {
                return Ok(LineRead::Timeout);
            }
        };
        if avail.is_empty() {
            break; // EOF：剩余数据（若有）按一行处理
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            if bytes.len() + pos > MAX_LINE {
                // P1-2：本行确认超长—— consume 到行尾后拒绝（含本块内换行前数据）
                reader.consume(pos + 1);
                return Ok(LineRead::TooLong);
            }
            bytes.extend_from_slice(&avail[..pos]);
            reader.consume(pos + 1);
            break;
        }
        if bytes.len() + avail.len() > MAX_LINE {
            // P1-2：加上本块必超长—— 本块整体消费后丢弃至行尾
            let n = avail.len();
            reader.consume(n);
            discard_to_newline(reader).await;
            return Ok(LineRead::TooLong);
        }
        let n = avail.len();
        bytes.extend_from_slice(avail);
        reader.consume(n);
    }
    if bytes.is_empty() {
        return Ok(LineRead::Eof);
    }
    Ok(LineRead::Line(String::from_utf8_lossy(&bytes).into_owned()))
}

/// 丢弃输入直到换行符（含）；EOF 或错误时停止
async fn discard_to_newline(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) {
    loop {
        match reader.fill_buf().await {
            Ok(s) if s.is_empty() => break,
            Ok(s) => match s.iter().position(|&b| b == b'\n') {
                Some(p) => {
                    reader.consume(p + 1);
                    break;
                }
                None => {
                    let n = s.len();
                    reader.consume(n);
                }
            },
            Err(_) => break,
        }
    }
}

async fn write_outbound(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    out: &Outbound,
) -> std::io::Result<()> {
    // P1-3：所有响应序列化进 Zeroizing 缓冲，写出后立即清零
    let buf: Zeroizing<Vec<u8>> = match out {
        Outbound::Plain(resp) => {
            let mut line = serde_json::to_string(resp)?;
            line.push('\n');
            Zeroizing::new(line.into_bytes())
        }
        Outbound::Creds { id, password } => {
            // 凭据响应手工拼接：不经过 serde 的中间 String，避免额外堆副本
            let mut line = Zeroizing::new(Vec::with_capacity(96 + password.len()));
            line.extend_from_slice(
                format!("{{\"id\":{id},\"ok\":true,\"payload\":{{\"username\":\"x-access-token\",\"password\":\"").as_bytes(),
            );
            push_json_escaped(&mut line, password);
            line.extend_from_slice(b"\"}}}\n");
            line
        }
    };
    writer.write_all(&buf).await?;
    writer.flush().await
}

/// 把 s 以 JSON 字符串转义后追加进 buf（P1-3：直接写入目标缓冲，不产生中间副本）
fn push_json_escaped(buf: &mut Vec<u8>, s: &str) {
    for c in s.chars() {
        match c {
            '"' => buf.extend_from_slice(b"\\\""),
            '\\' => buf.extend_from_slice(b"\\\\"),
            '\n' => buf.extend_from_slice(b"\\n"),
            '\r' => buf.extend_from_slice(b"\\r"),
            '\t' => buf.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => buf.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes()),
            c => {
                let mut b = [0u8; 4];
                buf.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
}

async fn dispatch(state: &Arc<DaemonState>, req: &Request, caller_pid: Option<u32>) -> Outbound {
    match req.cmd.as_str() {
        "status" => Outbound::Plain(cmd_status(state, req).await),
        "pubkey" => Outbound::Plain(cmd_pubkey(state, req)),
        "getuser" => Outbound::Plain(cmd_getuser(state, req)),
        "set_token" => Outbound::Plain(cmd_set_token(state, req).await),
        "get_pat" => cmd_get_pat(state, req, caller_pid).await,
        "gh" => Outbound::Plain(cmd_gh(state, req).await),
        "shutdown" => Outbound::Plain(Response::ok(req.id, json!({"ok": true}))),
        other => Outbound::Plain(Response::err(req.id, "BAD_REQUEST", format!("未知命令: {other}"))),
    }
}

async fn cmd_status(state: &Arc<DaemonState>, req: &Request) -> Response {
    let armed = state.page.lock().unwrap().is_armed();
    let fp = state.meta.lock().unwrap().fingerprint.clone();
    Response::ok(
        req.id,
        json!({
            "state": if armed { "armed" } else { "ready" },
            "fingerprint": fp,
        }),
    )
}

fn cmd_pubkey(state: &Arc<DaemonState>, req: &Request) -> Response {
    let pubkey = state.meta.lock().unwrap().pubkey.clone();
    Response::ok(req.id, json!({"pubkey": pubkey}))
}

/// 署名查询（wrap 注入用）：返回 start 时配置的 user.name/user.email，未配置为 null
fn cmd_getuser(state: &Arc<DaemonState>, req: &Request) -> Response {
    let meta = state.meta.lock().unwrap();
    Response::ok(req.id, json!({"user": meta.git_user, "email": meta.git_email}))
}

/// N-3：兼容 ASCII armored 的 age 密文（-----BEGIN AGE ENCRYPTED FILE-----）。
/// 非 armored 输入原样返回；armor 解包失败也原样返回，交由后续 Decryptor 报明确错误。
fn strip_age_armor(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else { return bytes.to_vec() };
    let t = text.trim();
    if !t.starts_with("-----BEGIN AGE ENCRYPTED FILE-----") {
        return bytes.to_vec();
    }
    let mut b64 = String::with_capacity(t.len());
    for line in t.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with("-----") {
            continue;
        }
        b64.push_str(l);
    }
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
        Ok(v) => v,
        Err(_) => bytes.to_vec(),
    }
}

/// set_token（§7.2 daemon 侧流程）
async fn cmd_set_token(state: &Arc<DaemonState>, req: &Request) -> Response {
    let Some(enc_b64) = &req.enc_b64 else {
        return Response::err(req.id, "BAD_REQUEST", "缺少 enc_b64");
    };
    use base64::Engine;
    let enc = match base64::engine::general_purpose::STANDARD.decode(enc_b64) {
        Ok(b) => b,
        Err(e) => return Response::err(req.id, "BAD_REQUEST", format!("base64 解码失败: {e}")),
    };
    let enc = strip_age_armor(&enc); // N-3

    // 1. 仅接受 Recipients 变体（拒绝 passphrase 加密）
    let decryptor = match age::Decryptor::new(&enc[..]) {
        Ok(age::Decryptor::Recipients(d)) => d,
        Ok(age::Decryptor::Passphrase(_)) => {
            return Response::err(req.id, Code::NotRecipientFormat.as_str(), "仅支持 age -r 公钥加密")
        }
        Err(_) => {
            return Response::err(req.id, Code::DecryptFailed.as_str(), "密文与公钥不匹配或已损坏")
        }
    };

    // 2. 以页内 identity 解密 → 临时缓冲（写入后立即 zeroize）
    let pat_tmp = {
        let page = state.page.lock().unwrap();
        let raw = page.identity_raw();
        let raw_z = zeroize::Zeroizing::new(raw);
        match crate::agekey::identity_from_raw(&raw_z) {
            Ok(id) => {
                let mut buf = Vec::new();
                let mut r = match decryptor.decrypt(std::iter::once(&id as &dyn age::Identity))
                {
                    Ok(r) => r,
                    Err(_) => {
                        return Response::err(
                            req.id,
                            Code::DecryptFailed.as_str(),
                            "密文与公钥不匹配或已损坏",
                        )
                    }
                };
                if std::io::Read::read_to_end(&mut r, &mut buf).is_err() {
                    return Response::err(req.id, Code::DecryptFailed.as_str(), "密文读取失败");
                }
                zeroize::Zeroizing::new(buf)
            }
            Err(e) => return Response::err(req.id, "IO", format!("identity 重建失败: {e}")),
        }
    };

    // PAT 不应包含首尾空白（人类 echo 管道常见尾随换行）
    let pat_str = match std::str::from_utf8(&pat_tmp) {
        Ok(s) => s.trim(),
        Err(_) => return Response::err(req.id, Code::DecryptFailed.as_str(), "明文非 UTF-8"),
    };
    if pat_str.is_empty() {
        return Response::err(req.id, Code::DecryptFailed.as_str(), "明文为空");
    }
    if pat_str.len() > crate::page::PAT_CAP {
        return Response::err(req.id, Code::PatTooLong.as_str(), format!("PAT 超长（>{} 字节）", crate::page::PAT_CAP));
    }

    // 3. GET /user 验证
    let resp = state
        .client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {pat_str}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => return Response::err(req.id, "API_ERROR", format!("访问 GitHub 失败: {e}")),
    };
    let status = resp.status().as_u16();
    // scopes：Classic PAT 从 X-OAuth-Scopes 头读取；Fine-grained 无此头 → "fine-grained"（§7.2）
    let scopes: Vec<String> = resp
        .headers()
        .get("X-OAuth-Scopes")
        .and_then(|v| v.to_str().ok())
        .map(parse_scopes)
        .unwrap_or_else(|| vec!["fine-grained".to_string()]);
    if status == 401 {
        return Response::err(req.id, Code::TokenInvalid.as_str(), "GitHub 返回 401");
    }
    if status != 200 {
        let msg = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
            v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string())
        }).unwrap_or_default();
        return Response::err(req.id, Code::ApiError.as_str(), format!("GitHub API {status}: {msg}"));
    }
    let user: serde_json::Value = match resp.json().await {
        Ok(u) => u,
        Err(e) => return Response::err(req.id, Code::ApiError.as_str(), format!("响应解析失败: {e}")),
    };
    let login = user.get("login").and_then(|v| v.as_str()).unwrap_or("?").to_string();

    // 4. 验证通过后：原地 zeroize 旧 PAT → 写入新 PAT → 记录指纹
    {
        let page = state.page.lock().unwrap();
        if page.set_pat(pat_str).is_err() {
            return Response::err(req.id, Code::PatTooLong.as_str(), "PAT 超长");
        }
    }
    let fp = fingerprint(pat_str);
    {
        let mut meta = state.meta.lock().unwrap();
        meta.login = Some(login.clone());
        meta.fingerprint = Some(fp.clone());
        meta.scopes = scopes.clone();
    }
    // N-4：审计行只含指纹，不含 PAT
    log_audit(&state.sock_path, "token_set", "ok", None, Some(&format!("login={login} fp={fp}")));
    Response::ok(
        req.id,
        json!({"login": login, "scopes": scopes, "fingerprint": fp}),
    )
}

/// get_pat（§6.3）：仅 cred-helper；host 限定 github.com 域。
/// P1-3：返回 Outbound::Creds，序列化走 Zeroizing 缓冲；
/// 出口副本收敛为单份 Zeroizing<String>（写出后随缓冲清零）。
/// 已知边界：cmd_gh 的 reqwest/auth 头路径为第三方库内部，无法保证清零（文档已标注）。
async fn cmd_get_pat(
    state: &Arc<DaemonState>,
    req: &Request,
    caller_pid: Option<u32>,
) -> Outbound {
    let host = req.host.as_deref().unwrap_or("");
    let protocol = req.protocol.as_deref().unwrap_or("");
    if protocol != "https" || !matches!(host, "github.com" | "www.github.com") {
        return Outbound::Plain(Response::err(
            req.id,
            Code::HostNotAllowed.as_str(),
            format!("不为 {host} 代理凭据"),
        ));
    }
    let pat = {
        let page = state.page.lock().unwrap();
        // IPC 传输必需的一次性拷贝；v0.0.2 起该副本为 Zeroizing，写出后清零
        page.pat().map(|s| Zeroizing::new(s.to_string()))
    };
    match pat {
        None => Outbound::Plain(Response::err(req.id, Code::NoToken.as_str(), "PAT 未注入")),
        Some(p) => {
            // N-4：审计行只含 host 与对端 PID，不含 PAT
            log_audit(
                &state.sock_path,
                "get_pat",
                "ok",
                caller_pid,
                Some(&format!("host={host}")),
            );
            Outbound::Creds { id: req.id, password: p }
        }
    }
}

async fn cmd_gh(state: &Arc<DaemonState>, req: &Request) -> Response {
    let Some(args) = &req.args else {
        return Response::err(req.id, "BAD_REQUEST", "缺少 args");
    };
    let pat = match state.page.lock().unwrap().pat() {
        // 借用需要 &str；此处短暂持有
        Some(p) => p.to_string(),
        None => return Response::err(req.id, Code::NoToken.as_str(), "PAT 未注入"),
    };
    let ctx = ApiCtx { client: &state.client, pat: &pat };
    let r: GhResult = gh_exec(&ctx, &args, req.repo.as_deref()).await;
    drop(pat);
    Response::ok(
        req.id,
        json!({"stdout": r.stdout, "stderr": r.stderr, "exit_code": r.exit_code}),
    )
}

fn gh_exec<'a>(
    ctx: &'a ApiCtx<'a>,
    args: &'a [String],
    repo: Option<&'a str>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = GhResult> + Send + 'a>> {
    Box::pin(crate::gh::execute(ctx, args, repo))
}

/// 处理 X-OAuth-Scopes 的辅助（set_token 用；保留以便 Fine-grained 判定）
pub fn parse_scopes(header: &str) -> Vec<String> {
    header.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// T-1：destroy 错误分支——unlink 目标不存在时也必须完成 zeroize
    #[test]
    fn destroy_parts_zeroizes_even_when_file_missing() {
        let page = Mutex::new(SensitivePage::new().unwrap());
        page.lock().unwrap().set_pat("tok_test_not_a_real_pat").unwrap();
        let dir = std::env::temp_dir().join(format!("ghpatd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        std::fs::write(&sock, b"x").unwrap();

        destroy_parts(&page, &sock);
        assert!(page.lock().unwrap().pat().is_none());
        assert!(!sock.exists());

        // 错误分支：文件已不存在，再次销毁不 panic 且保持清零
        destroy_parts(&page, &sock);
        assert!(page.lock().unwrap().pat().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N-3：armor 解包
    #[test]
    fn age_armor_stripped() {
        use base64::Engine;
        let payload = b"fake-ciphertext-for-armor-test";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let armored = format!(
            "-----BEGIN AGE ENCRYPTED FILE-----\n{b64}\n-----END AGE ENCRYPTED FILE-----\n"
        );
        assert_eq!(strip_age_armor(armored.as_bytes()), payload.to_vec());
        // 非 armored 原样返回
        assert_eq!(strip_age_armor(b"raw-bytes-no-armor"), b"raw-bytes-no-armor".to_vec());
        // armor 解包失败也原样返回（交由 Decryptor 报错）
        assert_eq!(
            strip_age_armor(b"-----BEGIN AGE ENCRYPTED FILE-----\n!!!\n-----END AGE ENCRYPTED FILE-----"),
            b"-----BEGIN AGE ENCRYPTED FILE-----\n!!!\n-----END AGE ENCRYPTED FILE-----".to_vec()
        );
    }

    /// P1-3：JSON 转义直写缓冲
    #[test]
    fn json_escape_works() {
        let mut buf = Vec::new();
        push_json_escaped(&mut buf, "a\"b\\c\n\r\t");
        assert_eq!(String::from_utf8(buf).unwrap(), "a\\\"b\\\\c\\n\\r\\t");
        let mut buf = Vec::new();
        push_json_escaped(&mut buf, "ghp_plain123");
        assert_eq!(String::from_utf8(buf).unwrap(), "ghp_plain123");
    }

    /// N-4：ISO8601 转换
    #[test]
    fn iso8601_known_values() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_791_018_000), "2026-10-03T09:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
