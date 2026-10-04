# ghpatd

面向云端 AI Agent 的 GitHub PAT 内存代理：在本地用一次性公钥加密 Token，手动复制密文发给云端 Agent 使用，全程不落盘、用后即焚。

## 使用说明

1. **让云端 Agent 启动并给出公钥**  
   复制 [docs/usage/PROMPT.md](docs/usage/PROMPT.md) 发送给云端 Agent（或让其安装 `ghpatd` 后执行 `ghpatd start`），获取输出的 `age1...` 公钥。

2. **在本地终端加密 PAT**  
   使用拿到的公钥加密你的 GitHub PAT，将输出的整段 `-----BEGIN AGE ENCRYPTED FILE-----` 文本密文复制发给云端 Agent：
   ```bash
   printf '%s' "ghp_xxxxxxxxxxxxxxxxxxxx" | age -r age1xxxxxxxx... -a
   ```

3. **云端注入与销毁**  
   - 注入密文：`ghpatd set-token <<'EOF' ... EOF`（密文前后若夹杂 markdown 围栏/说明文字会自动清洗；加 `--json` 可获得机器可读输出）
   - 执行 Git：`ghpatd wrap -- git pull / push / clone ...`
   - 执行 GitHub 操作：`ghpatd pr / issue / repo / api / auth ...`
   - 用完销毁：`ghpatd stop`

## 安全边界

- **已保护**：PAT 与私钥仅驻留内存，进程结束立即销毁；不写磁盘文件、不进命令行参数与环境变量、日志不记录明文；不同系统用户（UID）之间相互隔离。
- **边界限制**：
  - 防御目标是**避免凭据落盘与环境残留泄露**；云端 Agent 与 `ghpatd` 运行在同一用户权限下，无法从技术上阻止同用户进程主动调取凭据（需配合 [docs/usage/PROMPT.md](docs/usage/PROMPT.md) 约束 Agent 行为）。
  - 仅支持 `https://github.com` 仓库认证，不支持 SSH。

## 相关文档

- **使用文档（`docs/usage/`）**：[Prompt 模板](docs/usage/PROMPT.md) · [命令与 API](docs/usage/API.md) · [安全说明](docs/usage/SECURITY.md)
- **开发文档（`docs/dev/`）**：[需求说明](docs/dev/REQ.md) · [架构设计](docs/dev/ARCHITECTURE.md) · [实现规格](docs/dev/IMPLEMENTATION.md) · [复刻与测试](docs/dev/REPLICATION.md)
- **发布产物（`release/`）**：[二进制与校验和](release/) · [MIT-0 许可证](LICENSE)
