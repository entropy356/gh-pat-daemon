# ghpatd v0.0.2 — 面向 AI 智能体的 GitHub PAT 内存代理

单二进制实现（Rust，x86_64 Linux，musl 静态链接），按规格文档完整实现三形态：
`client` / `daemon(--daemon-internal)` / `cred-helper`。

> **v0.0.2 为 breaking 版本**：项目改名 `gh-pat-daemon`，命令名 `ghpat` → `ghpatd`，
> socket 路径同步改为 `${XDG_RUNTIME_DIR:-/tmp/ghpatd-$UID}/ghpatd.sock`。
> 升级后旧 `ghpat` 二进制与旧 socket 不再互通，需以 `ghpatd` 重新 start/set-token。

## 产物

| 文件 | 说明 |
|---|---|
| `ghpatd-linux-x86_64` | 主二进制，static-pie linked，stripped |
| `run_tests.sh` | 集成自测脚本（`bash run_tests.sh ./ghpatd-linux-x86_64`，37 用例） |
| `SHA256SUMS` | 全部产物校验和 |

## 快速上手

```bash
# 1. 启动 daemon（fork 子进程；打印 age 公钥）
./ghpatd-linux-x86_64 start
#    可选：./ghpatd-linux-x86_64 start --foreground   # 前台运行（调试）

# 2. 用打印出的公钥在本地加密 PAT（ghpatd 自身不把 PAT 写入磁盘、不进 argv/env）
#    v0.0.2 起推荐直接管道，pat.enc 不落盘：
age -r age1xxxxxxxx... -a pat.txt | ./ghpatd-linux-x86_64 set-token

# 3. 注入 daemon（校验 /user → 通过后写入 mlock 内存页；stdin 输入，兼容 ASCII armored）
./ghpatd-linux-x86_64 set-token < pat.enc     # 仅接受 stdin，不再接受文件路径

# 4. 日常使用
./ghpatd-linux-x86_64 status                    # 运行状态与指纹
./ghpatd-linux-x86_64 wrap -- git pull          # 以注入凭据执行任意命令
./ghpatd-linux-x86_64 api repos/o/r/issues      # GitHub API 透传
./ghpatd-linux-x86_64 stop                      # 销毁（zeroize 整页 + unlink socket）

# 5. git cred-helper（免 daemon 手工注入）
git -c credential.helper='!./ghpatd-linux-x86_64 cred-helper' clone https://github.com/o/r
```

## v0.0.2 变更记录

### 缺陷修复
- **P0-1** shutdown/stop 响应竞态：client 在响应写回前断开时，daemon 此前会跳过销毁
  （红队实测可复现：socket 残留、敏感页未清）。现改为响应 best-effort 写回，
  无论写回成败必执行 zeroize + unlink + exit；新增竞态回归用例。

### 加固（威胁模型 §8 不变）
- **P1-1** 连接读空闲超时 10s：异常客户端不再长期占用连接；仅关当前连接，daemon 不受影响。
- **P1-2** 请求行长上限 64KB：超长请求拒绝并关闭连接、记审计，防内存耗尽。
- **P1-3** get_pat 响应序列化改为手工拼接 + `Zeroizing` 缓冲，写出后立即清零，
  收缩 daemon 侧 PAT 堆上瞬态副本（`cmd_gh` 经第三方 HTTP 库的路径无法保证，见已知限制）。

### 功能变更
- **N-1** 项目改名 `gh-pat-daemon`，命令名 `ghpatd`；socket/log 文件名同步（breaking）。
- **N-2** `set-token` 仅接受 stdin，移除文件路径参数（pat.enc 不再需要落盘）。
- **N-3** 注入密文兼容 ASCII armored（`age -a` / `-----BEGIN AGE ENCRYPTED FILE-----`）
  与 base64 二进制两种编码，自动识别。
- **N-4** 审计日志统一为 `<ISO8601 UTC> action=<动作> result=<结果> peer_pid=<对端>` 格式；
  daemon 启动、token 注入、get_pat、shutdown、读超时、超长拒绝等事件全覆盖；日志无 PAT 明文。

### 测试
- 单元测试 5 → 9（新增 destroy 错误路径、armor 剥离、JSON 转义、ISO8601 转换）。
- 集成测试 23 → 37（新增：shutdown 竞态回归、读空闲超时、行长上限、文件参数移除、
  审计日志断言、PAT 明文泄漏检查）。

## 已知限制

1. **同 UID 进程可取回 PAT**（规格 §8 威胁模型内）：daemon 不隐藏内存、不混淆 IPC、
   `--daemon-internal` 可被同 UID 进程直接调用。`/proc/<pid>/mem` 读取与 restat 反查
   防护超出范围。本补丁的 P1 系列均为模型内收敛，不改变该边界。
2. **shutdown 无认证**（v0.0.2 决策，维持规格 §8）：同 UID 本可 `get_pat`，单独保护
   shutdown 收益有限；stop 前请确认。
3. **get_pat 返回真实 PAT**：cred-helper 与 `wrap` 场景下凭据进入 git/目标命令进程内存，
   依赖宿主进程自身纪律。
4. **PAT 堆副本不能完全消除**：P1-3 清零 daemon 自有缓冲；`cmd_gh` 请求头经 reqwest
   内部缓冲，无法保证 zeroize。
5. **GitHub token 中途失效**：401 不自动清零（保留现场便于诊断），需 `stop` 或重新注入。
6. **多用户并发**：仅支持单 UID；无第二 UID 的双用户隔离测试环境。
7. **daemon 启动时无父进程校验**：`--daemon-internal` 理论上可被同 UID 进程直接调用
   （同第 1 条边界）。
8. 与规格的偏差（均已在开工说明中记录）：IPC 增加 `pubkey` 命令；cred-helper
   兼容 `argv[1]==get` 与 `argv[2]==get` 两种布局；wrap 的 `GIT_CONFIG_COUNT` 采用累加偏移
   以兼容嵌套 wrap。
9. release 构建使用 `lto="thin"`（规格建议 fat，沙箱 2 核 + 网络文件系统无法承受 fat
   链接）。

## 文档（复刻级）

| 文档 | 内容 |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 总体架构、IPC 协议、敏感页内存模型、生命周期、威胁模型 |
| [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) | 逐模块实现规格（数据结构、函数签名、逐条行为规则、全部常量与算法） |
| [docs/REPLICATION.md](docs/REPLICATION.md) | 复刻步骤（12 步依赖顺序）、验收清单、集成测试规格、安全自查 |

三份文档合起来可在不参考源码的前提下完整复刻全部代码。

## 源码与构建

完整 Rust 工程位于本仓库 `ghpatd/` 目录（11 个模块，约 2700 行）：

```bash
cd ghpatd
cargo build --release        # 或 cross build --release --target x86_64-unknown-linux-musl
cargo test                   # 9 个单元测试
bash run_tests.sh ./target/release/ghpatd   # 37 个集成用例
```

- 模块划分：`page.rs`（mlock 敏感页）/ `agekey.rs`（age 密钥与加解密）/ `daemon.rs`（IPC 与
  生命周期）/ `client.rs` / `gh.rs`（gh 等价命令与 REST）/ `cred.rs` + `wrap.rs`（cred-helper
  与 wrap）/ `jq.rs` / `ipc.rs` / `err.rs` / `main.rs`（形态分叉）
- Cargo.toml 依赖与规格 §10 一致，另加 bech32 0.9 用于 age 密钥原始字节与页缓冲的互转；
  v0.0.2 起 tokio 启用 `time` feature（读空闲超时）
