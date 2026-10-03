#!/usr/bin/env bash
# ghpatd 集成自测（规格 §11.2 可执行项；v0.0.2 扩展：竞态/超时/行长/armored/审计日志）
# 用法: run_tests.sh <ghpatd二进制路径>
set -u
BIN="${1:?用法: run_tests.sh <ghpatd二进制>}"
TMP="$(mktemp -d /tmp/ghpatd-test-XXXXXX)"
SOCK="$TMP/run/ghpatd.sock"
LOG="$TMP/run/ghpatd.log"
mkdir -p "$TMP/run"
export XDG_RUNTIME_DIR="$TMP/run"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✔ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✘ $1"; }
check(){ if [ $? -eq 0 ]; then ok "$1"; else bad "$1"; fi; }

echo "== 1. 生命周期 =="
"$BIN" --sock "$SOCK" start >/dev/null 2>&1; check "start 退出码 0"
test -S "$SOCK"; check "socket 存在"
OUT="$("$BIN" --sock "$SOCK" pubkey)"
case "$OUT" in age1*) ok "pubkey 格式 age1…";; *) bad "pubkey 格式: $OUT";; esac
"$BIN" --sock "$SOCK" start >/dev/null 2>&1 && bad "重复 start 未报错" || ok "重复 start 报 DAEMON_ALREADY_RUNNING"
ST="$("$BIN" --sock "$SOCK" status)"
case "$ST" in *READY*) ok "status=READY";; *) bad "status: $ST";; esac

echo "== 2. argv 检查（§8.1: PAT 不出现在 argv）=="
FOUND=$(ps -eo args | grep -c "ghp_[t]est\|passwor[d]=" || true)
[ "$FOUND" = "0" ]; if [ "$FOUND" != "0" ]; then ps -eo args | grep "ghp_[t]est\|passwor[d]=" | head -3; fi
check "进程参数中无 PAT"

echo "== 3. set-token（stdin 注入，自加密 dummy token，GitHub 401 路径）=="
PUB="$("$BIN" --sock "$SOCK" pubkey | tail -1)"
echo -n "ghp_dummytoken_for_test_only_1234567890" > "$TMP/pat.txt"
if command -v age >/dev/null; then
  age -r "$PUB" -o "$TMP/token.enc" "$TMP/pat.txt"
else
  echo "（无 age CLI，使用二进制内置测试路径）"
  echo -n "not-a-valid-age-file" > "$TMP/token.enc"
fi
RES="$("$BIN" --sock "$SOCK" set-token < "$TMP/token.enc" 2>&1)"
if command -v age >/dev/null; then
  case "$RES" in *TOKEN_INVALID*|*"401"*) ok "假 token 被拒（TOKEN_INVALID）";; *) bad "set-token: $RES";; esac
else
  case "$RES" in *NOT_RECIPIENT_FORMAT*|*DECRYPT_FAILED*|*解密失败*|*仅支持) ok "非 age 密文被拒";; *) bad "set-token 错误路径: $RES";; esac
fi
ST="$("$BIN" --sock "$SOCK" status)"
case "$ST" in *READY*) ok "失败注入保留 READY";; *) bad "注入失败后状态: $ST";; esac

echo "== 4. cred-helper =="
OUT=$(printf 'protocol=https\nhost=gitlab.com\n\n' | "$BIN" cred-helper get 2>/dev/null)
[ -z "$OUT" ]; check "非 github.com host 输出为空"
OUT=$(printf 'protocol=https\nhost=github.com\n\n' | "$BIN" cred-helper get 2>/dev/null)
[ -z "$OUT" ]; check "未注入 token 时输出为空"
printf 'protocol=https\nhost=example.com\n\n' | "$BIN" cred-helper store; check "store 静默成功(退出码0)"
printf 'protocol=https\nhost=example.com\n\n' | "$BIN" cred-helper erase; check "erase 静默成功(退出码0)"

echo "== 5. wrap =="
"$BIN" --sock "$SOCK" wrap -- env > "$TMP/env.out" 2>&1
grep -q "^GIT_CONFIG_COUNT=1$" "$TMP/env.out"; check "GIT_CONFIG_COUNT=1"
grep -q "^GIT_CONFIG_KEY_0=credential.https://github.com.helper$" "$TMP/env.out"; check "KEY_0 注入"
grep -q "^GIT_TERMINAL_PROMPT=0$" "$TMP/env.out"; check "GIT_TERMINAL_PROMPT=0"
grep -q "^GHPATD_SOCK=$SOCK$" "$TMP/env.out"; check "GHPATD_SOCK 传递"
"$BIN" --sock "$SOCK" wrap -- true; check "wrap 透传退出码"

echo "== 6. N-2: set-token 文件参数已移除 =="
"$BIN" --sock "$SOCK" set-token "$TMP/token.enc" </dev/null >/dev/null 2>&1
[ $? -ne 0 ]; check "文件路径参数被拒绝（仅 stdin）"

echo "== 7. 崩溃恢复（kill -9 → 残留 socket）=="
DPID=$(pgrep -f "daemon-internal" | head -1)
kill -9 "$DPID" 2>/dev/null
sleep 0.3
"$BIN" --sock "$SOCK" status >/dev/null 2>&1 && bad "daemon 死后 status 未报错" || ok "daemon 死后 status 报错"
"$BIN" --sock "$SOCK" start >/dev/null 2>&1; check "残留 socket 清理后可重启"
"$BIN" --sock "$SOCK" stop >/dev/null 2>&1; check "stop 退出码 0"
test ! -e "$SOCK"; check "stop 后 socket 已 unlink"

echo "== 8. 权限 =="
D="$(dirname "$SOCK")"
STAT=$(stat -c '%a' "$D" 2>/dev/null || echo "")
[ "$STAT" = "700" ]; check "运行目录 0700（$STAT）"

echo "== 9. 多轮 start/stop 稳定性 =="
for i in 1 2 3; do
  "$BIN" --sock "$SOCK" start >/dev/null 2>&1 && "$BIN" --sock "$SOCK" stop >/dev/null 2>&1 || { bad "第 $i 轮 start/stop"; break; }
done
ok "3 轮 start/stop"

echo "== 10. P0-1: shutdown 竞态回归（响应写回前断开仍销毁）=="
"$BIN" --sock "$SOCK" start >/dev/null 2>&1; check "start（竞态用例前置）"
python3 - "$SOCK" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect(sys.argv[1])
s.sendall(b'{"id":999,"cmd":"shutdown"}\n')
s.close()  # 不读响应立即断开：v0.0.1 在此不销毁（红队缺陷 P0-1）
PY
DEAD=0
for i in $(seq 1 40); do
  [ ! -S "$SOCK" ] && DEAD=1 && break
  sleep 0.1
done
[ "$DEAD" = "1" ]; check "提前断开后 socket 仍被销毁"
pgrep -f "daemon-internal" >/dev/null 2>&1 && bad "daemon 进程仍存活" || ok "daemon 进程已退出"
"$BIN" --sock "$SOCK" start >/dev/null 2>&1; check "竞态销毁后可重启"

echo "== 11. P1-1: 读空闲超时（10s 无数据 → 关连接，daemon 存活）=="
TOUT=$(python3 - "$SOCK" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(20)
s.connect(sys.argv[1])
t0 = time.time()
try:
    data = s.recv(1024)
except Exception as e:
    data = b"EXC:" + str(e).encode()
dt = time.time() - t0
print("OK" if (data in (b"", b"EXC:timed out") and 8 < dt < 16) else f"BAD dt={dt:.1f} data={data!r}")
PY
)
[ "$TOUT" = "OK" ]; check "空闲连接 10s 被关闭（$TOUT）"
ST="$("$BIN" --sock "$SOCK" status)"
case "$ST" in *READY*) ok "超时后 daemon 仍 READY";; *) bad "超时后状态: $ST";; esac

echo "== 12. P1-2: 请求行长上限（>64KB 拒绝并关连接）=="
LCAP=$(python3 - "$SOCK" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
s.connect(sys.argv[1])
s.sendall(b'{"id":1,"cmd":"' + b'a'*70000 + b'"}\n')
try:
    data = s.recv(4096)
except Exception as e:
    data = b"EXC:" + str(e).encode()
print("OK" if b"BAD_REQUEST" in data else f"BAD {data[:60]!r}")
PY
)
[ "$LCAP" = "OK" ]; check "超长请求被拒（$LCAP）"
ST="$("$BIN" --sock "$SOCK" status)"
case "$ST" in *READY*) ok "超长拒绝后 daemon 仍 READY";; *) bad "超长后状态: $ST";; esac

echo "== 13. N-3: ASCII armored 兼容 =="
if command -v age >/dev/null; then
  age -r "$PUB" -a -o "$TMP/token.armored" "$TMP/pat.txt"
  RES="$("$BIN" --sock "$SOCK" set-token < "$TMP/token.armored" 2>&1)"
  case "$RES" in *TOKEN_INVALID*|*"401"*) ok "armored 密文解密成功并走 401 校验";; *) bad "armored: $RES";; esac
else
  echo "  （无 age CLI，跳过 armored 用例）"
fi

echo "== 14. N-4: 审计日志 =="
"$BIN" --sock "$SOCK" stop >/dev/null 2>&1
grep -q "action=shutdown" "$LOG"; check "日志含 shutdown 审计事件"
grep -q "action=conn_timeout" "$LOG"; check "日志含读超时审计事件"
grep -q "action=line_too_long" "$LOG"; check "日志含超长拒绝审计事件"
grep -q "action=token_set" "$LOG" && bad "失败注入不应记 token_set 成功" || ok "失败注入无 token_set 成功事件"
grep -q "ghp_dummytoken" "$LOG" && bad "日志泄漏 PAT" || ok "日志无 PAT 明文"

echo "== 15. git 署名注入 =="
"$BIN" --sock "$SOCK" start --user-name "AI Agent" --user-email "agent@example.com" >/dev/null 2>&1; check "start 携带署名重启"
"$BIN" --sock "$SOCK" wrap -- env > "$TMP/env_sig.out" 2>&1
grep -q "^GIT_CONFIG_COUNT=3$" "$TMP/env_sig.out"; check "配置署名后 GIT_CONFIG_COUNT=3"
grep -q "^GIT_CONFIG_KEY_1=user.name$" "$TMP/env_sig.out" && grep -q "^GIT_CONFIG_VALUE_1=AI Agent$" "$TMP/env_sig.out"; check "user.name 注入"
grep -q "^GIT_CONFIG_KEY_2=user.email$" "$TMP/env_sig.out" && grep -q "^GIT_CONFIG_VALUE_2=agent@example.com$" "$TMP/env_sig.out"; check "user.email 注入"
grep -q "^GIT_CONFIG_KEY_0=credential.https://github.com.helper$" "$TMP/env_sig.out"; check "凭据 helper 仍为条目 0"
"$BIN" --sock "$SOCK" stop >/dev/null 2>&1
"$BIN" --sock "$SOCK" start >/dev/null 2>&1
"$BIN" --sock "$SOCK" wrap -- env > "$TMP/env_nosig.out" 2>&1
grep -q "^GIT_CONFIG_COUNT=1$" "$TMP/env_nosig.out"; check "未配置署名时 GIT_CONFIG_COUNT=1（兼容默认行为）"
"$BIN" --sock "$SOCK" stop >/dev/null 2>&1

echo
echo "通过 $PASS / $((PASS+FAIL))"
[ "$FAIL" = "0" ] && echo "ALL_PASS" || echo "HAS_FAILURES"
rm -rf "$TMP"
exit "$FAIL"
