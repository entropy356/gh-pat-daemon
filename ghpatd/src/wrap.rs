//! wrap 形态（规格 §7.4）：注入 GIT_CONFIG_* + GIT_TERMINAL_PROMPT=0 + GHPATD_SOCK
//!
//! v0.0.2 署名支持：daemon 启动时可配置 user.name/user.email（start --user-name/--user-email），
//! wrap 通过 getuser 查询后一并注入 GIT_CONFIG 条目；未配置时行为与原版一致（仅凭据 helper）。

use crate::ipc::{self, Request};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// 查询 daemon 的 git 署名配置；daemon 不可达时按未配置处理（git 回落本地配置）
fn fetch_git_user(sock: &Path) -> (Option<String>, Option<String>) {
    let req = Request {
        id: ipc::next_id(),
        cmd: "getuser".into(),
        enc_b64: None,
        args: None,
        repo: None,
        host: None,
        protocol: None,
    };
    match ipc::call(sock, &req, Duration::from_secs(5)) {
        Ok(r) if r.ok => {
            let p = r.payload.unwrap_or(serde_json::Value::Null);
            let get = |k: &str| {
                p.get(k)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            };
            (get("user"), get("email"))
        }
        _ => (None, None),
    }
}

/// 由署名配置生成待追加的 GIT_CONFIG 条目（顺序：user.name → user.email）
pub fn git_user_entries(user: &Option<String>, email: &Option<String>) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(u) = user {
        v.push(("user.name".to_string(), u.clone()));
    }
    if let Some(e) = email {
        v.push(("user.email".to_string(), e.clone()));
    }
    v
}

pub fn run(sock: &Path, command: Vec<String>) -> i32 {
    if command.is_empty() {
        eprintln!("✘ wrap 需要 -- 后跟目标命令");
        return 1;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✘ 无法定位自身二进制: {e}");
            return 1;
        }
    };
    let helper_value = format!("!{} cred-helper", exe.display());

    // 兼容嵌套 wrap：在已有 GIT_CONFIG_COUNT 上累加，避免覆盖外层注入
    let existing: u32 = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let idx = existing;

    let (gu, ge) = fetch_git_user(sock);
    let extras = git_user_entries(&gu, &ge);

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .env("GIT_CONFIG_COUNT", (existing + 1 + extras.len() as u32).to_string())
        .env(format!("GIT_CONFIG_KEY_{idx}"), "credential.https://github.com.helper")
        .env(format!("GIT_CONFIG_VALUE_{idx}"), &helper_value)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GHPATD_SOCK", sock);
    let mut i = existing + 1;
    for (k, v) in &extras {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), k)
            .env(format!("GIT_CONFIG_VALUE_{i}"), v);
        i += 1;
    }

    match cmd.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => {
            eprintln!("✘ 启动目标命令失败: {e}");
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_user_no_entries() {
        assert!(git_user_entries(&None, &None).is_empty());
    }

    #[test]
    fn name_only() {
        let e = git_user_entries(&Some("AI Agent".into()), &None);
        assert_eq!(e, vec![("user.name".to_string(), "AI Agent".to_string())]);
    }

    #[test]
    fn both_ordered() {
        let e = git_user_entries(&Some("A".into()), &Some("a@b.c".into()));
        assert_eq!(
            e,
            vec![
                ("user.name".to_string(), "A".to_string()),
                ("user.email".to_string(), "a@b.c".to_string()),
            ]
        );
    }
}
