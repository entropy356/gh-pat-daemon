# 云端 AI Agent 安装与使用 Prompt 模板

> **说明**：本文档仅供人类用户复制后手动发送给云端 AI Agent，包含安装、公钥交接、密文注入与安全约束说明。

````markdown
请在当前 Linux 环境中安装并使用 `ghpatd`（GitHub PAT 内存代理），按以下步骤操作：

1. 安装 `ghpatd` 到 `$PATH`：
```bash
curl -fsSLO https://github.com/entropy356/gh-pat-daemon/releases/download/v0.0.2/ghpatd-linux-x86_64
curl -fsSLO https://github.com/entropy356/gh-pat-daemon/releases/download/v0.0.2/api.md
install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd || sudo install -m 0755 ./ghpatd-linux-x86_64 /usr/local/bin/ghpatd
```
（完整命令与参数参考下载的 `api.md`。）

2. 启动 daemon 并把公钥发给我：
```bash
ghpatd start
# 如需配置 git 提交署名：ghpatd start --user-name "<Name>" --user-email "<Email>"
```
将输出的 `age1...` 公钥发给我，等待我在本地用 `age -r <公钥> -a` 加密 PAT 后把 ASCII armored 密文发给你。
（如需机器解析，可改用 `ghpatd --json start` 获取 JSON 输出。）

3. 收到我发来的密文后，通过 `stdin` 注入（严禁落盘）：
```bash
ghpatd set-token <<'EOF'
-----BEGIN AGE ENCRYPTED FILE-----
...
-----END AGE ENCRYPTED FILE-----
EOF
ghpatd status
```
（密文外若夹杂代码围栏或说明文字会被自动清洗，但仍建议直接粘贴纯密文；可用 `ghpatd --json set-token` 校验注入结果。）

4. 日常使用与销毁：
- Git 远程操作：`ghpatd wrap -- git pull`、`ghpatd wrap -- git push`、`ghpatd wrap -- git clone https://github.com/owner/repo.git`
- GitHub 操作：`ghpatd auth status`、`ghpatd repo view`、`ghpatd pr list`、`ghpatd pr create ...`、`ghpatd issue list`、`ghpatd api <endpoint>`
- 任务完成后销毁内存凭据：`ghpatd stop`

5. 安全铁律（必须严格遵守）：
- **严禁尝试获取、读取、打印、记录或导出 PAT 明文**。
- **严禁主动调用 `ghpatd cred-helper get` 或直接向 socket 发送 `get_pat` 请求读取 token**。
- **严禁向我索要 PAT 明文，也严禁将收到的密文或任何凭据写入磁盘文件、环境变量或日志**。
- 只能通过 `ghpatd wrap -- git ...` 或 `ghpatd <repo|pr|issue|api|auth>` 间接使用凭据。
````
