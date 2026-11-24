#!/usr/bin/env bash
# 构建 fcitx5 插件并跑按键行为测试。
#
#   scripts/addon-test.sh [lexicon.tsv] [english.txt]
#   WOWTYPE_VOICE_WAV=说的话.wav WOWTYPE_VOICE_EXPECT=期望的字 scripts/addon-test.sh ...  # 加跑语音
#   WOWTYPE_HARNESS_DBUS=$DBUS_SESSION_BUS_ADDRESS ...   # 让插件连上会话总线（测语音浮条的信号）
#
# 起的是一个私有、无界面、无 D-Bus 的 fcitx5 实例（配置目录都在临时目录里），
# 不会影响正在用的输入法。不给词库时用仓库里的 demo 词典，部分用例会因为缺词失败。
set -euo pipefail
cd "$(dirname "$0")/.."

LEXICON=${1:-$PWD/core/data/demo_lexicon.tsv}
ENGLISH=${2:-}
BUILD=$(realpath -m "${WOWTYPE_ADDON_TEST_BUILD:-fcitx5-addon/build-test}")

cargo build --release -p wowtype-core
cmake -S fcitx5-addon -B "$BUILD" -DCMAKE_BUILD_TYPE=Release -DWOWTYPE_BUILD_TESTS=ON >/dev/null
cmake --build "$BUILD" -j

# 设置契约：插件的键名、默认值、范围、可选值和 spec/config.json 对得上（顺带对照设置界面）
python3 scripts/test-config-contract.py --fcitx5 "$BUILD/wowtype-config-dump"

ROOT=$(mktemp -d)
MOCK_PID=
DBUS_PID=
SYNC_PID=
trap '[[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null; [[ -n "$DBUS_PID" ]] && kill "$DBUS_PID" 2>/dev/null; [[ -n "$SYNC_PID" ]] && kill "$SYNC_PID" 2>/dev/null; rm -rf "$ROOT"' EXIT

# 外部命令切模式的用例要会话总线：没给 WOWTYPE_HARNESS_DBUS 时起一条私有的（没有 dbus-daemon 时跳过）
if [[ -z "${WOWTYPE_HARNESS_DBUS:-}" ]] && command -v dbus-daemon >/dev/null; then
  WOWTYPE_HARNESS_DBUS=$(dbus-daemon --session --fork --print-address=1 --print-pid=3 3>"$ROOT/dbus.pid")
  DBUS_PID=$(cat "$ROOT/dbus.pid")
fi

# 互译用例：本机起一个假的 OpenAI 服务（没有 python3 或 curl 时跳过）
TRANSLATE_URL=
if command -v python3 >/dev/null && command -v curl >/dev/null; then
  python3 fcitx5-addon/test/mock_openai.py > "$ROOT/mock.port" &
  MOCK_PID=$!
  for _ in $(seq 50); do [[ -s "$ROOT/mock.port" ]] && break; sleep 0.1; done
  TRANSLATE_URL="http://127.0.0.1:$(head -1 "$ROOT/mock.port")/v1"
  echo sk-harness > "$ROOT/translate.key"
fi
mkdir -p "$ROOT/data/fcitx5/addon" "$ROOT/data/fcitx5/inputmethod" "$ROOT/config/fcitx5"
cp "$BUILD/wowtype-addon.conf" "$ROOT/data/fcitx5/addon/wowtype.conf"
cp "$BUILD/wowtype.conf" "$ROOT/data/fcitx5/inputmethod/wowtype.conf"
# fcitx5 自带的测试输入法，用来模拟「打到一半切换输入法」
cat > "$ROOT/data/fcitx5/addon/testim.conf" <<'ADDON'
[Addon]
Name=testim
Category=InputMethod
Library=libtestim
Type=SharedLibrary
OnDemand=False
ADDON
# 假的 clipboard 插件（测试程序里注册的静态插件），盖掉系统的那个。
# WOWTYPE_HARNESS_REAL_CLIPBOARD=1 时用系统装的 clipboard 插件，只确认调用它不出错
if [[ -z "${WOWTYPE_HARNESS_REAL_CLIPBOARD:-}" ]]; then
cat > "$ROOT/data/fcitx5/addon/clipboard.conf" <<'ADDON'
[Addon]
Name=clipboard
Category=Module
Library=clipboard
Type=StaticLibrary
OnDemand=False
ADDON
fi
cat > "$ROOT/config/fcitx5/profile" <<'PROFILE'
[Groups/0]
Name=Default
Default Layout=us
DefaultIM=wowtype

[Groups/0/Items/0]
Name=wowtype
Layout=

[Groups/0/Items/1]
Name=testim
Layout=

[GroupOrder]
0=Default
PROFILE

# 语音用例要真模型：给了 WOWTYPE_VOICE_WAV（代替麦克风的录音）才跑，模型默认用本机已下载的那份。
# 没给时用一个空目录，跑「没有模型时按语音键只提示、不下载」
VOICE_WAV=${WOWTYPE_VOICE_WAV:-}
if [[ -n "$VOICE_WAV" ]]; then
  VOICE_MODEL_DIR=${WOWTYPE_VOICE_MODEL_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/wowtype/wtlocal}
else
  VOICE_MODEL_DIR="$ROOT/data/wowtype/wtlocal"
fi

# 跨设备复制粘贴：三台「设备」配成一组——测试里的输入法（sync.json）、命令行起的另一台（一直听着，
# 收到什么写进 sync-peer.out）、第三台（测试里用 send 往输入法送）。都在本机，各用各的端口。
# 没有 python3（挑空闲端口）时跳过
SYNC_BIN=$PWD/target/release/wowtype-sync
SYNC_PEER_OUT=
SYNC_SEND=
SYNC_PORT=
SYNC_PEER_PORT=
if [[ -x "$SYNC_BIN" ]] && command -v python3 >/dev/null; then
  read -r SYNC_PORT SYNC_PEER_PORT < <(python3 - <<'PORTS'
import socket
socks = [socket.socket() for _ in range(2)]
for s in socks:
    s.bind(("127.0.0.1", 0))
print(*(s.getsockname()[1] for s in socks))
PORTS
  )
  sync_pair() {  # sync_pair <主持的配对文件> <主持的名字> <加入的配对文件> <加入的名字>
    "$SYNC_BIN" pair-host --state "$1" --name "$2" > "$ROOT/pair.out" &
    local host=$!
    for _ in $(seq 100); do grep -q '^code ' "$ROOT/pair.out" && break; sleep 0.1; done
    local code port
    code=$(awk '/^code/{print $2}' "$ROOT/pair.out")
    port=$(awk '/^code/{print $4}' "$ROOT/pair.out"); port=${port##*:}
    "$SYNC_BIN" pair-join "$code" --state "$3" --name "$4" --host "127.0.0.1:$port" >/dev/null
    wait "$host"
  }
  sync_pair "$ROOT/sync-peer.json" 另一台 "$ROOT/sync.json" 这台
  sync_pair "$ROOT/sync.json" 这台 "$ROOT/sync-third.json" 第三台
  SYNC_PEER_OUT=$ROOT/sync-peer.out
  "$SYNC_BIN" listen --state "$ROOT/sync-peer.json" --name 另一台 --port "$SYNC_PEER_PORT" \
    --peer "127.0.0.1:$SYNC_PORT" > "$SYNC_PEER_OUT" &
  SYNC_PID=$!
  for _ in $(seq 100); do grep -q '^ready ' "$SYNC_PEER_OUT" && break; sleep 0.1; done
  # 测试程序用 sh 跑这一句（printf 把 \\n 变成真的换行）
  SYNC_SEND="'$SYNC_BIN' send \"\$(printf '第三台复制的\\n换行')\" --state '$ROOT/sync-third.json' --name 第三台 --host 127.0.0.1:$SYNC_PORT >/dev/null"
fi

SYSTEM_ADDONS=$(pkg-config --variable=libdir Fcitx5Core 2>/dev/null || echo /usr/lib/x86_64-linux-gnu)/fcitx5
env -i PATH="$PATH" HOME="$ROOT" \
  XDG_DATA_HOME="$ROOT/data" XDG_CONFIG_HOME="$ROOT/config" XDG_RUNTIME_DIR="$ROOT" \
  XDG_DATA_DIRS="/usr/share" \
  FCITX_ADDON_DIRS="$BUILD:$SYSTEM_ADDONS" \
  WOWTYPE_LEXICON="$LEXICON" WOWTYPE_ENGLISH="$ENGLISH" WOWTYPE_LM="${WOWTYPE_LM:-}" WOWTYPE_LOG="${WOWTYPE_LOG:-0}" \
  WOWTYPE_USER_DICT="$ROOT/user.tsv" \
  WOWTYPE_HARNESS_TRANSLATE_URL="$TRANSLATE_URL" WOWTYPE_TRANSLATE_KEY_FILE="$ROOT/translate.key" \
  WOWTYPE_VOICE_WAV="$VOICE_WAV" WOWTYPE_VOICE_MODEL_DIR="$VOICE_MODEL_DIR" \
  DBUS_SESSION_BUS_ADDRESS="${WOWTYPE_HARNESS_DBUS:-}" \
  WOWTYPE_HARNESS_REAL_CLIPBOARD="${WOWTYPE_HARNESS_REAL_CLIPBOARD:-}" \
  WOWTYPE_SYNC_STATE_FILE="$ROOT/sync.json" WOWTYPE_SYNC_LAN_PORT="$SYNC_PORT" \
  WOWTYPE_SYNC_PEERS="${SYNC_PEER_PORT:+127.0.0.1:$SYNC_PEER_PORT}" WOWTYPE_SYNC_BEACON_MS=200 \
  WOWTYPE_HARNESS_SYNC_PEER_OUT="$SYNC_PEER_OUT" WOWTYPE_HARNESS_SYNC_SEND="$SYNC_SEND" \
  WOWTYPE_NO_UPDATE_CHECK=1 WOWTYPE_HARNESS_SHOW="${WOWTYPE_HARNESS_SHOW:-}" WOWTYPE_VOICE_EXPECT="${WOWTYPE_VOICE_EXPECT:-}" \
  "$BUILD/wowtype-key-harness"
