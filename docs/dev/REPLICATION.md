# ghpatd 复刻指南与测试规格（v0.0.2）

> 复刻级规格之三。配合 [ARCHITECTURE.md](ARCHITECTURE.md)（架构/协议/内存模型）与
> [IMPLEMENTATION.md](IMPLEMENTATION.md)（逐模块规格）使用：前者定"是什么"，中者定"怎么写"，
> 本文定"按什么顺序写、写到什么程度算复刻成功"。

## 1. 复刻前置条件

| 项 | 要求 |
|---|---|
| 工具链 | Rust ≥1.75（2021 edition）、cargo；交叉编译需 `cross` 或 musl-gcc（`x86_64-unknown-linux-musl` target） |
| 运行环境 | Linux x86_64；mlock 可用；测试需 `bash`、`python3`；armored 用例需 `age` CLI（缺失自动跳过） |
| 网络 | `set-token` 验证与 `gh` 子命令需访问 api.github.com |

## 2. 复刻顺序（依赖驱动，共 12 步）

每步的详细规格见 IMPLEMENTATION.md 对应章节；步骤间仅沿下列依赖方向推进：

```
① Cargo 骨架 → ② err.rs → ③ page.rs → ④ agekey.rs → ⑤ ipc.rs → ⑥ jq.rs
→ ⑦ daemon.rs（最大模块，可再按 4.2→4.9→4.4→4.3 内部顺序）→ ⑧ client.rs
→ ⑨ gh.rs → ⑩ cred.rs → ⑪ wrap.rs → ⑫ main.rs + run_tests.sh
```

1. **骨架**：按 IMPLEMENTATION §0 建 crate（bin 名 `ghpatd`），声明 11 个 mod；此时
   `cargo build` 通过（模块先用空实现占位）。
2. **err.rs**：Code 枚举 + as_str + Display + client_message（§10）。无依赖，供全员引用。
3. **page.rs**：SensitivePage + fingerprint + 2 个单测（§1）。只依赖 libc/zeroize。
   验收：`cargo test page` 通过。
4. **agekey.rs**：generate / raw_from_bech32 / identity_from_raw / encrypt_for_test + 2 个单测（§2）。
   验收：`cargo test agekey` 通过（roundtrip 与加解密闭环）。
5. **ipc.rs**：Request/Response/Outbound/next_id + 路径解析 + call + probe_connect（§3）。
6. **jq.rs**：apply + 1 个单测（§9）。
7. **daemon.rs**：按顺序实现 log/iso8601 → read_line_capped/discard_to_newline →
   write_outbound/push_json_escaped → strip_age_armor → 各 cmd_* → dispatch → handle_conn →
   run_internal → destroy_parts + 4 个单测（§4）。此步完成即得到可运行的 daemon：
   `GHPATD_SOCK=… ./ghpatd --daemon-internal` 应打印 `OK age1…`，nc 发 `{"id":1,"cmd":"status"}`
   应回 `{"id":1,"ok":true,…}`。
8. **client.rs**：start（fork/exec 自身）/ set_token / stop / status / pubkey / gh / resolve_repo（§5）。
9. **gh.rs**：api_call/api_list/表格/`--json` 投影 + 12 个子命令实现（§6）。纯 daemon 内逻辑，
   可用单测覆盖表格与字段映射（当前实现未单测，属可选）。
10. **cred.rs / wrap.rs**：均为无状态薄层（§7、§8）。
11. **main.rs**：argv 分叉（cred-helper、--daemon-internal 先于 clap）+ 全部子命令声明（§11）。
12. **run_tests.sh**：按 §4 用例表重写集成脚本。

> 关键实现纪律（复刻时最常见的失真点）：
> - daemon 启动序列次序不可调换（umask → PR_SET_DUMPABLE → 建页 → 密钥 → bind → OK）；
> - shutdown 必"无论响应写回成败必 destroy"（P0-1）；
> - set_token 任何失败路径不得改页内状态；401 不清零；
> - 凭据响应走手工 JSON 拼接 + Zeroizing，不用 serde；
> - 读空闲超时用 `tokio::time::timeout` 包 `fill_buf`（tokio UnixStream 无 read_timeout）。

## 3. 交付物验收清单

| # | 验收项 | 通过标准 |
|---|---|---|
| A1 | `cargo build --release` | 0 warning 0 error |
| A2 | `cargo test` | 9 个单元测试全过：page 2 + agekey 2 + daemon 4 + jq 1 |
| A3 | `bash run_tests.sh <bin>` | 全部用例通过，末行 `ALL_PASS`，退出码 0 |
| A4 | musl 静态性 | `file` 输出 `static-pie linked`；`ldd` 报 not dynamic |
| A5 | socket/日志权限 | 运行目录 0700，sock 与 log 0600 |
| A6 | argv 纪律 | `ps -eo args` 全程无 PAT/明文密码 |
| A7 | 日志纪律 | 日志无 PAT 明文；含 §4-T14 所列审计事件 |
| A8 | 手工 E2E | start → age 加密真 token → set-token → wrap git push 成功 → stop |

## 4. 集成测试规格（run_tests.sh，14 组 / 37 条断言）

环境：`mktemp -d` 隔离，`XDG_RUNTIME_DIR=$TMP/run`，全部经 `--sock` 定位。
公共断言器：`ok/bad/check`（`check` 依据上一命令退出码）。末行输出 `通过 N / N` 与
`ALL_PASS`/`HAS_FAILURES`，退出码 = 失败数。

| 组 | 名称 | 断言 |
|---|---|---|
| T1 | 生命周期（5） | start 退出码 0；socket 存在；pubkey 匹配 `age1*`；重复 start 报 DAEMON_ALREADY_RUNNING；status 含 READY |
| T2 | argv 检查（1） | `ps -eo args` 无 `ghp_…`/`password=` |
| T3 | set-token 错误路径（3） | 有 age：假 token 被 401 拒（TOKEN_INVALID）；无 age：坏密文被拒（NOT_RECIPIENT_FORMAT/DECRYPT_FAILED）；失败后 status 仍 READY |
| T4 | cred-helper（4） | 非 github.com host 空输出；未注入时空输出；store 静默 0；erase 静默 0 |
| T5 | wrap（5） | `wrap -- env`：GIT_CONFIG_COUNT=1、KEY_0=credential.https://github.com.helper、GIT_TERMINAL_PROMPT=0、GHPATD_SOCK=$SOCK；`wrap -- true` 透传退出码 |
| T6 | 仅 stdin（1） | `set-token <路径>` 被拒绝（非零退出） |
| T7 | 崩溃恢复（4） | kill -9 后 status 报错；残留 socket 清理后可重启；stop 退出码 0；stop 后 socket unlink |
| T8 | 权限（1） | 运行目录 0700 |
| T9 | 稳定性（1） | 连续 3 轮 start/stop |
| T10 | P0-1 竞态回归（4） | python 发 shutdown 后立即断开 → 4s 内 socket 消失；进程退出；随后可重启 |
| T11 | P1-1 空闲超时（2） | 连接后静默，10±2s 内被关；daemon 仍 READY |
| T12 | P1-2 行长上限（2） | 发 >64KB 行被拒（收到错误响应）；daemon 仍 READY |
| T13 | N-3 armored（1，条件） | `age -a` 密文 → set-token 走到 TOKEN_INVALID（证明 armor 剥离成功） |
| T14 | N-4 审计日志（5） | 日志含 shutdown、conn_timeout、line_too_long 事件；失败注入无 token_set 成功事件；日志无 PAT 明文 |
| T15 | 署名注入（6） | `start --user-name --user-email` 后 `wrap -- env`：GIT_CONFIG_COUNT=3、KEY_1=user.name、KEY_2=user.email、KEY_0 凭据 helper 不变；无署名重启后 COUNT=1（默认行为兼容） |

## 5. 手工 E2E 剧本（验收项 A8，云端 Agent + 本地手动复制密文）

```bash
# 1. [云端 Agent] 启动 daemon，将打印的 age1… 公钥发给用户
./ghpatd start

# 2. [用户本地] 用公钥加密 PAT 生成 ASCII armored 文本，复制整段密文发送给云端 Agent
printf '%s' "<你的PAT>" | age -r <公钥> -a

# 3. [云端 Agent] 将收到的密文经 stdin（heredoc）注入 daemon，全程不落盘
./ghpatd set-token <<'EOF'
-----BEGIN AGE ENCRYPTED FILE-----
...
-----END AGE ENCRYPTED FILE-----
EOF

# 4. [云端 Agent] 校验状态、执行 git 操作并销毁
./ghpatd status                       # ARMED (fingerprint: …)
./ghpatd --sock $SOCK wrap -- git push origin main
./ghpatd stop                         # 已销毁；确认 socket 已消失
```

## 6. 与原规格的偏差记录（复刻者须知）

1. IPC 新增 `pubkey` 命令（便于脚本化取公钥，不破坏原协议）。
2. cred-helper 兼容 `argv[1]==get` 与 `argv[1]==cred-helper, argv[2]==get` 两种布局。
3. wrap 的 `GIT_CONFIG_COUNT` 采用累加偏移（兼容嵌套 wrap）。
4. release `lto="thin"`（规格建议 fat；资源受限环境不可行）。
5. Cargo.toml 较规格 §10 多 `bech32 0.9`（age 私钥原始字节互转）；tokio 启用 `time`（读超时）。
6. tokio `UnixStream` 无 `set_read_timeout`，P1-1 以 `tokio::time::timeout` 实现（规格未指明实现方式）。

## 7. 安全自查清单（复刻完成后逐条核对）

- [ ] PAT 唯一权威存储在 mlock 页，页外副本全部 Zeroizing 或写出即清零
- [ ] umask 先于 socket bind；PR_SET_DUMPABLE 先于密钥生成
- [ ] shutdown 竞态：响应写回失败仍 destroy（T10 过）
- [ ] SO_PEERCRED uid 校验拒绝跨 UID 连接
- [ ] set_token 全失败路径状态不变；401 不清零
- [ ] 审计日志零 PAT 明文；指纹只到末 4 字符
- [ ] cred-helper 对 store/erase 静默成功，不落盘
- [ ] get_pat 域过滤：仅 https + github.com/www.github.com
