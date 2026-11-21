#!/usr/bin/env bash
# 无网络、无 root、无真实包管理操作的安装器回归测试。
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/assets"

# 拦截所有可能修改系统/输入法配置的命令；curl 只读取本地夹具。
cat > "$work/bin/mock" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
name=${0##*/}
case "$name" in
  sudo) exec "$@" ;;
  # GitHub 的地址从本地夹具取：FIXTURE_MIRROR 是测试里的代理前缀（去掉前缀再认地址），
  # FIXTURE_DIRECT_DOWN=1 时直连 GitHub 像被重置一样失败（curl 35），
  # FIXTURE_MIRROR_FOLLOWS=1 时代理自己跟着跳转，releases/latest 不跳到 tag 地址、直接返回发布页
  curl)
    output= url= writeout=
    while (($#)); do
      case "$1" in
        -o) output=$2; shift 2 ;;
        -w) writeout=1; shift 2 ;;
        --connect-timeout|--max-time) shift 2 ;;
        *) url=$1; shift ;;
      esac
    done
    printf 'curl %s\n' "$url" >> "$FIXTURE_FETCH_LOG"
    direct=$url via=direct gh=https://github.com/Gloridust/WowType
    if [[ -n "${FIXTURE_MIRROR:-}" && "$url" == "$FIXTURE_MIRROR"* ]]; then direct=${url#"$FIXTURE_MIRROR"} via=mirror; fi
    if [[ "$via" == direct && "$direct" == https://* && -n "${FIXTURE_DIRECT_DOWN:-}" ]]; then
      echo 'curl: (35) Recv failure: Connection reset by peer' >&2; exit 35
    fi
    case "$direct" in
      file://*) [[ "$via" == direct ]] || { echo "代理不该转发本地地址：$url" >&2; exit 1; }
        cp -- "${url#file://}" "$output" ;;
      "$gh/releases/latest")
        if [[ "$via" == mirror && -n "${FIXTURE_MIRROR_FOLLOWS:-}" ]]; then
          if [[ -n "$writeout" ]]; then printf '%s\n' "$url"
          else printf '<a href="/Gloridust/WowType/releases/tag/v0.3.8">WowType v0.3.8</a>\n'; fi
        else
          printf '%s\n' "${url%/latest}/tag/v0.3.8"
        fi ;;
      "$gh/releases/download/v0.3.8/"*) cp -- "$FIXTURE_ASSETS/${direct##*/}" "$output" || exit 22 ;;
      "$gh/archive/refs/tags/v0.3.8.tar.gz") cp -- "$FIXTURE_SOURCE" "$output" || exit 22 ;;
      *) printf '测试禁止联网：%s\n' "$url" >&2; exit 1 ;;
    esac
    ;;
  pgrep) exit 1 ;;
  # GNOME 扩展：FIXTURE_EXTENSIONS / FIXTURE_ENABLED 是装了的、启用了的；FIXTURE_KIMPANEL=1 时软件源里有 kimpanel
  gnome-extensions)
    case "${1:-}" in
      list) if [[ "${2:-}" == --enabled ]]; then printf '%s' "${FIXTURE_ENABLED:-}"; else printf '%s' "${FIXTURE_EXTENSIONS:-}"; fi ;;
      *) printf 'gnome-extensions %s\n' "$*" >> "$FIXTURE_LOG"; exit "${FIXTURE_ENABLE_EXIT:-0}" ;;
    esac
    ;;
  apt-cache) [[ -n "${FIXTURE_KIMPANEL:-}" ]] ;;
  gsettings)
    printf 'gsettings %s\n' "$*" >> "$FIXTURE_LOG"
    [[ "${1:-}" != get ]] || printf '%s\n' "${FIXTURE_GSETTINGS:-@as []}"
    ;;
  im-config) [[ "${1:-}" != -m ]] || printf 'fcitx5\nfcitx5\n' ;;
  # openSUSE 源码编译：cargo 要在解开的源码目录里跑，cmake --install 时词库要已经解到 dist/data
  cargo) [[ -f fcitx5-addon/CMakeLists.txt ]] || { echo "cargo 不在源码目录里跑：$PWD" >&2; exit 1; } ;&
  cmake)
    if [[ "$name" == cmake && "${1:-}" == --install ]]; then
      [[ -f "${2%/build}/dist/data/lexicon.tsv" ]] || { echo "词库没解到 dist/data" >&2; exit 1; }
    fi
    ;&
  apt-get|pacman|dnf|zypper|wowtype-config|installer)
    printf '%s' "$name" >> "$FIXTURE_LOG"
    printf ' %q' "$@" >> "$FIXTURE_LOG"
    printf '\n' >> "$FIXTURE_LOG"
    # wowtype-config --setup 的退出码：0 好了、2 要注销重新登录、3 刚重启了 fcitx5、4 还差一步、1 出错
    [[ "$name $*" != "wowtype-config --setup"* ]] || exit "${FIXTURE_SETUP_EXIT:-0}"
    ;;
  *) printf '意外命令：%s\n' "$name" >&2; exit 1 ;;
esac
MOCK
chmod +x "$work/bin/mock"
for name in sudo curl apt-get pacman dnf zypper cargo cmake pgrep im-config wowtype-config installer gnome-extensions apt-cache gsettings; do
  ln -s mock "$work/bin/$name"
done

test_arch=x86_64
test_version=v0.3.8
test_system=Linux
test_no_switch=1
test_desktop=
test_base="file://$work/assets"
test_source="file://$work/source.tar.gz"
test_mirror=
count=0
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { count=$((count + 1)); printf 'ok %s - %s\n' "$count" "$*"; }
# macOS 自带的 bash 3.2 在 UTF-8 下把变量名后面紧跟的全角字符（比如全角括号）当成变量名的一部分（set -u 时直接报
# unbound variable）。变量后面紧跟非 ASCII 字符的一律要写成 ${version}。CI 的 bash 5 复现不了，只能这样查。
bad=$(cd "$root" && git ls-files -z '*.sh' macos/pkg/scripts | xargs -0 perl -ne 'print "$ARGV:$.: $_" if /\$[A-Za-z_]\w*[^\x00-\x7f]/; close ARGV if eof' || true)
[[ -z "$bad" ]] || fail "变量后面紧跟中文要加花括号（macOS 的 bash 3.2 会报 unbound variable）：
$bad"

run() {
  env -u SUDO_USER PATH="$work/bin:$PATH" \
    FIXTURE_LOG="$work/commands" FIXTURE_FETCH_LOG="$work/fetches" \
    WOWTYPE_SYSTEM="$test_system" WOWTYPE_OS_RELEASE="$work/os-release" WOWTYPE_ARCH="$test_arch" \
    WOWTYPE_VERSION="$test_version" WOWTYPE_REPO=Gloridust/WowType \
    WOWTYPE_BASE_URL="$test_base" WOWTYPE_SOURCE_URL="$test_source" WOWTYPE_YES=1 \
    WOWTYPE_GITHUB_MIRROR="$test_mirror" FIXTURE_MIRROR=https://mirror.test/ \
    FIXTURE_ASSETS="$work/assets" FIXTURE_SOURCE="$work/source.tar.gz" \
    WOWTYPE_NO_IM_CONFIG="$test_no_switch" WOWTYPE_NO_RESTART="$test_no_switch" XDG_CURRENT_DESKTOP="$test_desktop" \
    bash "$root/install.sh" "$@"
}
reset() {
  : > "$work/commands"
  : > "$work/fetches"
  rm -f "$work/assets/"* "$work/source.tar.gz"
  test_arch=x86_64
  test_version=v0.3.8
  test_system=Linux
  test_no_switch=1
  test_desktop=
  test_base="file://$work/assets"
  test_source="file://$work/source.tar.gz"
  test_mirror=
  unset FIXTURE_SETUP_EXIT FIXTURE_EXTENSIONS FIXTURE_ENABLED FIXTURE_KIMPANEL FIXTURE_ENABLE_EXIT FIXTURE_GSETTINGS
  unset FIXTURE_DIRECT_DOWN FIXTURE_MIRROR_FOLLOWS
  printf '%s\n' "$1" > "$work/os-release"
}
asset() {
  printf 'WowType fixture for %s\n' "$1" > "$work/assets/$1"
  (cd "$work/assets" && sha256sum "$1") >> "$work/assets/SHA256SUMS"
}
plan() {
  reset "$1"
  local result
  result=$(run --print-plan) || fail "计划失败：$1"
  [[ "$result" == *"$2"* ]] || fail "包选择错误：$result"
  [[ "$result" == *"包管理器：$3"* ]] || fail "包管理器错误：$result"
  [[ ! -s "$work/commands" && ! -s "$work/fetches" ]] || fail '--print-plan 有副作用'
  pass "$4"
}
rejected() {
  reset "$1"
  if run --print-plan > "$work/output" 2>&1; then fail "不应支持：$1"; fi
  grep -Fq "$2" "$work/output" || fail "缺少有效指引：$(cat "$work/output")"
  [[ ! -s "$work/commands" && ! -s "$work/fetches" ]] || fail '拒绝平台时有副作用'
  pass "$3"
}
assert_log() { grep -Fq -- "$1" "$work/commands" || fail "缺少命令：$1"; }
assert_no_install() {
  if grep -Eq '^(apt-get|pacman|dnf|zypper|cargo|cmake|installer) ' "$work/commands"; then fail '校验失败仍运行包管理器'; fi
}

plan $'ID=ubuntu\nVERSION_ID="22.04"' wowtype_0.3.8_ubuntu22.04_amd64.deb apt-get 'Ubuntu 22.04'
plan $'ID=linuxmint\nVERSION_ID="21.3"\nID_LIKE="ubuntu debian"\nUBUNTU_CODENAME=jammy' ubuntu22.04 apt-get 'Linux Mint 21 uses Ubuntu 22.04 base'
plan $'ID=ubuntu\nVERSION_ID="24.04"' wowtype_0.3.8_ubuntu24.04_amd64.deb apt-get 'Ubuntu 24.04'
plan $'ID=deepin\nVERSION_ID="25"\nVERSION_CODENAME=crimson' wowtype_0.3.8_deepin25_amd64.deb apt-get 'deepin 25'
plan $'ID=deepin\nVERSION_ID="25.0.9"' wowtype_0.3.8_deepin25_amd64.deb apt-get 'deepin 25 point release'
plan $'ID=deepin\nVERSION_ID="23"\nVERSION_CODENAME=beige' wowtype_0.3.8_deepin23_amd64.deb apt-get 'deepin 23'
plan $'ID=uos\nVERSION_ID="25"' wowtype_0.3.8_deepin25_amd64.deb apt-get 'UOS V25 uses the deepin 25 package'
plan $'ID=uos\nVERSION_ID="2500"' wowtype_0.3.8_deepin25_amd64.deb apt-get 'UOS V25 build number'
plan $'ID=ubuntu\nVERSION_ID="26.04"' wowtype_0.3.8_ubuntu26.04_amd64.deb apt-get 'Ubuntu 26.04'
plan $'ID=debian\nVERSION_ID="13.1"' wowtype_0.3.8_debian13_amd64.deb apt-get 'Debian 13 point release'
plan $'ID=linuxmint\nVERSION_ID="22.1"\nID_LIKE="ubuntu debian"\nUBUNTU_CODENAME=noble' ubuntu24.04 apt-get 'Linux Mint uses Ubuntu base'
plan $'ID=pop\nVERSION_ID="24.04"\nID_LIKE="ubuntu debian"\nVERSION_CODENAME=noble' ubuntu24.04 apt-get 'Pop!_OS uses explicit base'
plan $'ID=example\nID_LIKE=debian\nDEBIAN_CODENAME=trixie' debian13 apt-get 'Debian derivative with explicit trixie base'
plan 'ID=arch' wowtype-0.3.8-1-x86_64.pkg.tar.zst pacman 'Arch Linux'
plan $'ID=endeavouros\nID_LIKE=arch' pkg.tar.zst pacman 'EndeavourOS'
plan $'ID=garuda\nID_LIKE=arch' pkg.tar.zst pacman 'Garuda'
plan $'ID=cachyos\nID_LIKE=arch' pkg.tar.zst pacman 'CachyOS'
plan $'ID=fedora\nVERSION_ID=43' wowtype-0.3.8-1.fc43.x86_64.rpm dnf 'Fedora version-specific RPM'
rejected $'ID=ubuntu\nVERSION_ID=20.04' '源码' 'Ubuntu 20.04 is not silently upgraded to a newer binary'
rejected $'ID=Deepin\nVERSION_ID="20.8"\nVERSION_CODENAME="apricot"' 'GTK 4' 'deepin 20 rejected with reason'
rejected $'ID=uos\nVERSION_ID="20"\nVERSION_CODENAME="eagle"' 'GTK 4' 'UOS 20 rejected with reason'
rejected $'ID=linuxmint\nVERSION_ID=22\nID_LIKE="ubuntu debian"' '基线' 'Unknown derivative base rejected'
rejected $'ID=manjaro\nID_LIKE=arch' 'package-arch.sh' 'Manjaro uses native source package'
rejected $'ID=example\nID_LIKE="arch manjaro"' 'package-arch.sh' 'Manjaro derivative rejected'
rejected $'ID=fedora\nVERSION_ID=43\nVARIANT_ID=silverblue' 'rpm-ostree' 'Silverblue uses rpm-ostree'
rejected $'ID=fedora\nVERSION_ID=43\nVARIANT_ID=sway-atomic' 'rpm-ostree' 'Fedora Atomic uses rpm-ostree'
rejected 'ID=nixos' 'flake' 'NixOS uses flake'
plan $'ID=opensuse-tumbleweed\nID_LIKE="opensuse suse"\nVERSION_ID=20261120' 'WowType-0.3.8 源码' zypper 'openSUSE Tumbleweed builds from source'
rejected $'ID=opensuse-leap\nID_LIKE="suse opensuse"\nVERSION_ID=16.0' '源码' 'openSUSE Leap source guidance'
rejected $'ID=rocky\nID_LIKE="rhel centos fedora"\nVERSION_ID=9' '源码' 'RHEL derivative is not treated as Fedora'
rejected 'ID=alpine' '源码' 'Alpine source guidance'

# arm64：deb 和 rpm 有预编译包，Arch 没有
arm_plan() { # <os-release> <uname -m> <期望包名> <说明>
  reset "$1"
  test_arch=$2
  local result
  result=$(run --print-plan) || fail "计划失败：$1 $2"
  [[ "$result" == *"$3"* ]] || fail "包选择错误：$result"
  pass "$4"
}
arm_plan $'ID=ubuntu\nVERSION_ID="24.04"' aarch64 wowtype_0.3.8_ubuntu24.04_arm64.deb 'Ubuntu 24.04 arm64'
arm_plan $'ID=debian\nVERSION_ID="13"' arm64 wowtype_0.3.8_debian13_arm64.deb 'Debian 13 arm64 (uname arm64)'
arm_plan $'ID=fedora\nVERSION_ID=44' aarch64 wowtype-0.3.8-1.fc44.aarch64.rpm 'Fedora aarch64 RPM'
arm_plan $'ID=ubuntu\nVERSION_ID="22.04"' aarch64 wowtype_0.3.8_ubuntu22.04_arm64.deb 'Ubuntu 22.04 arm64'

reset $'ID=deepin\nVERSION_ID="25"'
test_arch=aarch64
if run --print-plan > "$work/output" 2>&1; then fail 'Unexpected aarch64 deepin binary'; fi
grep -Fq 'x86_64' "$work/output" || fail 'Missing deepin arch guidance'
pass 'deepin aarch64 builds from source'

reset 'ID=arch'
test_arch=aarch64
if run --print-plan > "$work/output" 2>&1; then fail 'Unexpected aarch64 Arch binary'; fi
grep -Fq 'package-arch.sh' "$work/output" || fail 'Missing Arch aarch64 guidance'
pass 'Arch aarch64 builds locally'

reset $'ID=opensuse-tumbleweed\nID_LIKE="opensuse suse"'
result=$(run --print-plan) || fail 'openSUSE 计划失败'
[[ "$result" == *'编译依赖：'*'fcitx5-devel'*'typelib-1_0-Adw-1'* ]] || fail "openSUSE 计划没列编译依赖：$result"
[[ ! -s "$work/commands" && ! -s "$work/fetches" ]] || fail 'openSUSE --print-plan 有副作用'
pass 'openSUSE plan lists zypper build dependencies'

reset $'ID=ubuntu\nVERSION_ID="24.04"'
test_arch=riscv64
if run --print-plan > "$work/output" 2>&1; then fail 'Unexpected riscv64 binary'; fi
grep -Fq '源码' "$work/output" || fail 'Missing architecture guidance'
pass 'Architecture mismatch rejected'

reset "ID=\$(touch $work/unexpected)"
if run --print-plan > "$work/output" 2>&1; then fail 'Unsafe os-release accepted'; fi
[[ ! -e "$work/unexpected" ]] || fail 'os-release shell code executed'
pass 'os-release is parsed as data'

reset 'ID=arch'
test_version=
result=$(run --print-plan)
[[ "$result" == *'wowtype-<版本>-1-x86_64.pkg.tar.zst'* && ! -s "$work/fetches" ]] || fail 'Default plan needs network'
pass 'Plan with unspecified version stays offline'

reset $'ID=ubuntu\nVERSION_ID=24.04'
asset wowtype_0.3.8_ubuntu24.04_amd64.deb
asset wowtype-data-rime-ice_20260101_all.deb
run > "$work/output" 2>&1 || fail "apt install: $(cat "$work/output")"
assert_log 'apt-get update -qq'
assert_log 'apt-get install -y'
assert_log 'fcitx5-config-qt fcitx5-frontend-all im-config'
assert_log 'python3-gi python3-gi-cairo gir1.2-gtk-4.0 gir1.2-adw-1'
assert_log 'wowtype_0.3.8_ubuntu24.04_amd64.deb'
assert_log 'wowtype-data-rime-ice_20260101_all.deb'
assert_log 'wowtype-config --setup --yes --no-im-config --no-restart'
pass 'APT installs verified local packages and legacy data'

reset $'ID=ubuntu\nVERSION_ID=22.04'
asset wowtype_0.3.8_ubuntu22.04_amd64.deb
run > "$work/output" 2>&1 || fail "apt install 22.04: $(cat "$work/output")"
assert_log 'fcitx5-config-qt fcitx5-frontend-gtk3 fcitx5-frontend-gtk4 fcitx5-frontend-qt5 im-config'
if grep -Fq 'fcitx5-frontend-all' "$work/commands"; then fail '22.04 没有 fcitx5-frontend-all'; fi
assert_log 'wowtype_0.3.8_ubuntu22.04_amd64.deb'
pass 'Ubuntu 22.04 installs the frontends it has'

reset $'ID=deepin\nVERSION_ID=25\nVERSION_CODENAME=crimson'
asset wowtype_0.3.8_deepin25_amd64.deb
run > "$work/output" 2>&1 || fail "apt install deepin: $(cat "$work/output")"
assert_log 'wowtype_0.3.8_deepin25_amd64.deb'
assert_log 'fcitx5-frontend-all python3-gi'
if grep -Eq '(^| )im-config( |$)|fcitx5-config-qt' "$work/commands"; then fail 'deepin 不该装 im-config、fcitx5-config-qt'; fi
assert_log 'wowtype-config --setup --yes --no-im-config --no-restart'
pass 'deepin keeps its own IM framework and config tool'

# 输入法框架、输入法列表交给 wowtype-config --setup：不设 WOWTYPE_NO_* 时不带对应的选项，退出码决定最后的提示
reset $'ID=ubuntu\nVERSION_ID=24.04'
asset wowtype_0.3.8_ubuntu24.04_amd64.deb
test_no_switch=
export FIXTURE_SETUP_EXIT=2
run > "$work/output" 2>&1 || fail "setup relogin: $(cat "$work/output")"
grep -Eq '^wowtype-config --setup --yes$' "$work/commands" || fail "--setup 的选项不对：$(cat "$work/commands")"
grep -Fq '注销并重新登录' "$work/output" || fail "缺少注销提示：$(cat "$work/output")"
FIXTURE_SETUP_EXIT=3 run > "$work/output" 2>&1 || fail "setup restarted: $(cat "$work/output")"
grep -Fq 'Chromium/Electron' "$work/output" || fail "缺少重启提示：$(cat "$work/output")"
FIXTURE_SETUP_EXIT=1 run > "$work/output" 2>&1 || fail "setup failed 时安装不该失败：$(cat "$work/output")"
grep -Fq '一键设置' "$work/output" || fail "缺少一键设置提示：$(cat "$work/output")"
pass 'Setup is delegated to wowtype-config --setup'

reset $'ID=deepin\nVERSION_ID=25\nVERSION_CODENAME=crimson'
asset wowtype_0.3.8_deepin25_amd64.deb
test_no_switch=
run > "$work/output" 2>&1 || fail "deepin setup: $(cat "$work/output")"
grep -Eq '^wowtype-config --setup --yes --no-im-config$' "$work/commands" || fail "deepin 不该切输入法框架：$(cat "$work/commands")"
pass 'deepin setup never switches the IM framework'

# GNOME：软件源里有 kimpanel 就装上并启用；没有的只给链接；装了没启用的只启用
reset $'ID=debian\nVERSION_ID=13'
asset wowtype_0.3.8_debian13_amd64.deb
test_desktop=ubuntu:GNOME
export FIXTURE_KIMPANEL=1 FIXTURE_ENABLE_EXIT=1 FIXTURE_GSETTINGS="['ding@rastersoft.com']"
run > "$work/output" 2>&1 || fail "kimpanel: $(cat "$work/output")"
assert_log 'apt-get install -y gnome-shell-extension-kimpanel'
assert_log 'gnome-extensions enable kimpanel@kde.org'
assert_log "gsettings set org.gnome.shell enabled-extensions ['ding@rastersoft.com', 'kimpanel@kde.org']"
grep -Fq '注销并重新登录' "$work/output" || fail "装了扩展要提示重新登录：$(cat "$work/output")"
pass 'GNOME installs and enables kimpanel from the distro repository'

reset $'ID=ubuntu\nVERSION_ID=24.04'
asset wowtype_0.3.8_ubuntu24.04_amd64.deb
test_desktop=ubuntu:GNOME
run > "$work/output" 2>&1 || fail "kimpanel link: $(cat "$work/output")"
if grep -q 'kimpanel' "$work/commands"; then fail "软件源里没有 kimpanel 时不该装：$(cat "$work/commands")"; fi
grep -Fq 'extensions.gnome.org/extension/261' "$work/output" || fail "缺少扩展链接：$(cat "$work/output")"
pass 'GNOME without a kimpanel package keeps the link'

reset $'ID=ubuntu\nVERSION_ID=24.04'
asset wowtype_0.3.8_ubuntu24.04_amd64.deb
test_desktop=GNOME
export FIXTURE_EXTENSIONS=$'kimpanel@kde.org\n'
run > "$work/output" 2>&1 || fail "kimpanel enable: $(cat "$work/output")"
assert_log 'gnome-extensions enable kimpanel@kde.org'
if grep -q 'apt-get install -y gnome-shell' "$work/commands"; then fail '装了的扩展不该再装'; fi
export FIXTURE_ENABLED=$'kimpanel@kde.org\n'
: > "$work/commands"
run > "$work/output" 2>&1 || fail "kimpanel enabled: $(cat "$work/output")"
if grep -q 'gnome-extensions\|gsettings' "$work/commands"; then fail '已经启用了不该再动'; fi
pass 'GNOME only enables an installed kimpanel when needed'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
asset wowtype-data-rime-ice_20260101_all.deb
run > "$work/output" 2>&1 || fail "pacman install: $(cat "$work/output")"
assert_log 'pacman -S --needed --noconfirm'
if grep -q 'pacman -Syu' "$work/commands"; then fail 'installer ran a full system upgrade'; fi
assert_log 'python-gobject gtk4 libadwaita'
assert_log 'pacman -U --noconfirm'
if grep -q 'wowtype-data-rime-ice' "$work/fetches"; then fail 'Arch fetched a Debian data package'; fi
pass 'Pacman installs GUI dependencies and local package without -Syu'

reset $'ID=fedora\nVERSION_ID=43'
asset wowtype-0.3.8-1.fc43.x86_64.rpm
run > "$work/output" 2>&1 || fail "dnf install: $(cat "$work/output")"
assert_log 'dnf install -y'
assert_log 'wowtype-0.3.8-1.fc43.x86_64.rpm'
assert_log 'fcitx5-gtk3 fcitx5-gtk4 fcitx5-qt5 fcitx5-qt6'
pass 'DNF installs exact Fedora RPM and GTK/Qt frontends'

# openSUSE：zypper 装编译依赖，源码用这一版 tag 的压缩包，词库用发布页上校验过的词库包
opensuse_assets() {
  mkdir -p "$work/tree/WowType-0.3.8/fcitx5-addon" "$work/tree/data"
  : > "$work/tree/WowType-0.3.8/fcitx5-addon/CMakeLists.txt"
  printf '你好\tnihao\t1\n' > "$work/tree/data/lexicon.tsv"
  tar -czf "$work/source.tar.gz" -C "$work/tree" WowType-0.3.8
  tar -czf "$work/assets/wowtype-data-rime-ice-2026.1.1.1.tar.gz" -C "$work/tree/data" lexicon.tsv
  (cd "$work/assets" && sha256sum wowtype-data-rime-ice-2026.1.1.1.tar.gz) >> "$work/assets/SHA256SUMS"
  rm -rf "$work/tree"
}
reset $'ID=opensuse-tumbleweed\nID_LIKE="opensuse suse"'
opensuse_assets
asset wowtype-data-rime-ice_20260101_all.deb
run > "$work/output" 2>&1 || fail "openSUSE install: $(cat "$work/output")"
assert_log 'zypper --non-interactive refresh'
assert_log 'zypper --non-interactive install -y'
assert_log 'fcitx5-devel fcitx5-configtool fcitx5-gtk3 fcitx5-gtk4 fcitx5-qt5 fcitx5-qt6 alsa-devel'
assert_log 'python3-gobject python3-gobject-Gdk typelib-1_0-Gtk-4_0 typelib-1_0-Adw-1'
assert_log 'cargo build --release --locked -p wowtype-core'
assert_log 'cmake -S fcitx5-addon -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr'
assert_log 'cmake --install'
assert_log 'wowtype-config --setup --yes --no-im-config --no-restart'
grep -Fq "curl file://$work/source.tar.gz" "$work/fetches" || fail 'openSUSE 没下载源码包'
if grep -q 'wowtype-data-rime-ice_' "$work/fetches"; then fail 'openSUSE fetched a Debian data package'; fi
[[ "$(grep -n '^zypper .*install' "$work/commands" | cut -d: -f1)" -lt "$(grep -n '^cargo ' "$work/commands" | cut -d: -f1)" ]] ||
  fail '要先装依赖再编译'
pass 'openSUSE installs build deps with zypper and builds the release source with its lexicon'

reset $'ID=opensuse-tumbleweed\nID_LIKE="opensuse suse"'
opensuse_assets
printf 'corruption\n' >> "$work/assets/wowtype-data-rime-ice-2026.1.1.1.tar.gz"
if run > "$work/output" 2>&1; then fail 'Corrupted openSUSE lexicon accepted'; fi
assert_no_install
pass 'openSUSE lexicon checksum mismatch prevents zypper and the build'

reset $'ID=opensuse-tumbleweed\nID_LIKE="opensuse suse"'
asset wowtype-0.3.8-1.fc43.x86_64.rpm
if run > "$work/output" 2>&1; then fail 'openSUSE release without a lexicon tarball accepted'; fi
grep -Fq '词库包' "$work/output" || fail "缺少词库包提示：$(cat "$work/output")"
assert_no_install
pass 'openSUSE release without a lexicon tarball fails before zypper'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
printf 'corruption\n' >> "$work/assets/wowtype-0.3.8-1-x86_64.pkg.tar.zst"
if run > "$work/output" 2>&1; then fail 'Corrupted asset accepted'; fi
assert_no_install
pass 'Checksum mismatch prevents all package operations'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
cat "$work/assets/SHA256SUMS" > "$work/duplicate"
cat "$work/duplicate" >> "$work/assets/SHA256SUMS"
if run > "$work/output" 2>&1; then fail 'Duplicate hashes accepted'; fi
assert_no_install
pass 'Duplicate manifest entries rejected'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
printf 'bad  wowtype-0.3.8-1-x86_64.pkg.tar.zst\n' > "$work/assets/SHA256SUMS"
if run > "$work/output" 2>&1; then fail 'Malformed hash accepted'; fi
assert_no_install
pass 'Malformed manifest hash rejected'

reset $'ID=fedora\nVERSION_ID=44'
asset wowtype-0.3.8-1.fc43.x86_64.rpm
if run > "$work/output" 2>&1; then fail 'Wrong Fedora version accepted'; fi
assert_no_install
pass 'Missing target release asset fails before package operations'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
rm "$work/assets/wowtype-0.3.8-1-x86_64.pkg.tar.zst"
if run > "$work/output" 2>&1; then fail 'Missing asset accepted'; fi
assert_no_install
pass 'Failed asset download prevents package operations'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
sed -i 's/  / */' "$work/assets/SHA256SUMS"
run > "$work/output" 2>&1 || fail 'Binary-mode checksum line rejected'
pass 'Binary-mode SHA256SUMS supported'

reset 'ID=arch'
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
test_version=
run > "$work/output" 2>&1 || fail 'Latest release resolution failed'
grep -Fq 'https://github.com/Gloridust/WowType/releases/latest' "$work/fetches" || fail 'Wrong release repository'
pass 'Latest release uses the upstream repository'

# 连不上 GitHub：WOWTYPE_GITHUB_MIRROR 给 GitHub 的地址加代理前缀，不设时什么都不变
gh_dl=https://github.com/Gloridust/WowType/releases/download/v0.3.8
mirror_dl=https://mirror.test/$gh_dl
only_mirror() { # 除了 <允许的直连地址>，下载记录里的 GitHub 地址都要带代理前缀
  if grep -v -Fx "curl $1" "$work/fetches" | grep -v '^curl https://mirror.test/https://' | grep -q .; then
    fail "有地址没走代理：$(cat "$work/fetches")"
  fi
}

reset 'ID=arch'
test_base=
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
run > "$work/output" 2>&1 || fail "no mirror: $(cat "$work/output")"
grep -Fxq "curl $gh_dl/SHA256SUMS" "$work/fetches" || fail "SHA256SUMS 地址不对：$(cat "$work/fetches")"
grep -Fxq "curl $gh_dl/wowtype-0.3.8-1-x86_64.pkg.tar.zst" "$work/fetches" || fail "安装包地址不对：$(cat "$work/fetches")"
if grep -q 'mirror.test' "$work/fetches" || grep -Fq '代理' "$work/output"; then fail '没设代理却走了代理'; fi
assert_log 'pacman -U --noconfirm'
pass 'Without a mirror GitHub URLs are fetched unchanged'

reset 'ID=arch'
test_base=
test_version=
test_mirror=https://mirror.test
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
run > "$work/output" 2>&1 || fail "mirror: $(cat "$work/output")"
grep -Fxq 'curl https://mirror.test/https://github.com/Gloridust/WowType/releases/latest' "$work/fetches" ||
  fail "查最新版没走代理：$(cat "$work/fetches")"
grep -Fxq "curl $gh_dl/SHA256SUMS" "$work/fetches" || fail "SHA256SUMS 该先直连：$(cat "$work/fetches")"
grep -Fxq "curl $mirror_dl/wowtype-0.3.8-1-x86_64.pkg.tar.zst" "$work/fetches" || fail "安装包没走代理：$(cat "$work/fetches")"
only_mirror "$gh_dl/SHA256SUMS"
grep -Fq '第三方代理 https://mirror.test/' "$work/output" || fail "缺少第三方代理提醒：$(cat "$work/output")"
grep -Fq 'SHA256SUMS 直接从 GitHub 下载' "$work/output" || fail "缺少直连提示：$(cat "$work/output")"
assert_log 'pacman -U --noconfirm'
pass 'Mirror prefixes every GitHub download, SHA256SUMS tries GitHub first'

reset 'ID=arch'
test_base=
test_mirror=https://mirror.test/
export FIXTURE_DIRECT_DOWN=1
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
run > "$work/output" 2>&1 || fail "mirror fallback: $(cat "$work/output")"
grep -Fxq "curl $mirror_dl/SHA256SUMS" "$work/fetches" || fail "SHA256SUMS 没改走代理：$(cat "$work/fetches")"
only_mirror "$gh_dl/SHA256SUMS"
grep -Fq '改从代理下载' "$work/output" || fail "缺少改走代理的说明：$(cat "$work/output")"
assert_log 'pacman -U --noconfirm'
pass 'SHA256SUMS falls back to the mirror and says so'

reset 'ID=arch'
test_base=
test_mirror=https://mirror.test/
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
printf 'corruption\n' >> "$work/assets/wowtype-0.3.8-1-x86_64.pkg.tar.zst"
if run > "$work/output" 2>&1; then fail 'Corrupted asset accepted through mirror'; fi
grep -Fq '校验和对不上' "$work/output" || fail "缺少校验失败提示：$(cat "$work/output")"
assert_no_install
pass 'Checksums are still enforced through a mirror'

reset 'ID=arch'
test_base=
test_version=
test_mirror=https://mirror.test/
export FIXTURE_MIRROR_FOLLOWS=1
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
run > "$work/output" 2>&1 || fail "mirror follows redirects: $(cat "$work/output")"
grep -Fxq "curl $mirror_dl/wowtype-0.3.8-1-x86_64.pkg.tar.zst" "$work/fetches" || fail "没认出最新版：$(cat "$work/fetches")"
only_mirror "$gh_dl/SHA256SUMS"
pass 'Latest tag is read from the release page when the mirror follows redirects'

reset 'ID=arch'
test_mirror=https://mirror.test/
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
run > "$work/output" 2>&1 || fail "mirror with local assets: $(cat "$work/output")"
if grep -q 'mirror.test' "$work/fetches"; then fail "不是 GitHub 的地址不该加代理：$(cat "$work/fetches")"; fi
pass 'Mirror leaves non-GitHub asset URLs alone'

reset $'ID=opensuse-tumbleweed\nID_LIKE="opensuse suse"'
test_base=
test_source=
test_mirror=https://mirror.test/
opensuse_assets
run > "$work/output" 2>&1 || fail "openSUSE mirror: $(cat "$work/output")"
grep -Fxq 'curl https://mirror.test/https://github.com/Gloridust/WowType/archive/refs/tags/v0.3.8.tar.gz' "$work/fetches" ||
  fail "源码包没走代理：$(cat "$work/fetches")"
grep -Fxq "curl $mirror_dl/wowtype-data-rime-ice-2026.1.1.1.tar.gz" "$work/fetches" || fail "词库包没走代理：$(cat "$work/fetches")"
only_mirror "$gh_dl/SHA256SUMS"
assert_log 'cmake --install'
pass 'openSUSE source and lexicon tarballs go through the mirror'

reset 'ID=unused'
test_system=Darwin
test_base=
test_mirror=https://mirror.test/
asset WowType-0.3.8-macos.pkg
run > "$work/output" 2>&1 || fail "macOS mirror: $(cat "$work/output")"
grep -Fxq "curl $mirror_dl/WowType-0.3.8-macos.pkg" "$work/fetches" || fail "macOS 包没走代理：$(cat "$work/fetches")"
only_mirror "$gh_dl/SHA256SUMS"
assert_log 'installer -pkg'
pass 'macOS pkg goes through the mirror'

reset 'ID=arch'
test_mirror=https://mirror.test
result=$(run --print-plan) || fail '代理计划失败'
[[ "$result" == *'GitHub 代理：https://mirror.test/'* && ! -s "$work/fetches" ]] || fail "计划里没写代理：$result"
pass 'Plan shows the mirror and stays offline'

for bad_mirror in 'ftp://mirror.test/' 'https://mirror test/' "https://mirror.test/\$(id)"; do
  reset 'ID=arch'
  test_mirror=$bad_mirror
  if run --print-plan > "$work/output" 2>&1; then fail "不该接受代理地址：$bad_mirror"; fi
  grep -Fq 'WOWTYPE_GITHUB_MIRROR' "$work/output" || fail "缺少代理地址格式提示：$(cat "$work/output")"
done
pass 'Malformed mirror prefixes are rejected'

# 没设代理、GitHub 连不上：给出 WOWTYPE_GITHUB_MIRROR 和 jsDelivr 的用法
reset 'ID=arch'
test_base=
export FIXTURE_DIRECT_DOWN=1
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
if run > "$work/output" 2>&1; then fail 'GitHub 连不上还装成功了'; fi
grep -Fq 'WOWTYPE_GITHUB_MIRROR=https://<代理地址>/' "$work/output" || fail "缺少代理用法提示：$(cat "$work/output")"
grep -Fq 'https://cdn.jsdelivr.net/gh/Gloridust/WowType@main/install.sh' "$work/output" || fail "缺少 jsDelivr 提示：$(cat "$work/output")"
assert_no_install
test_version=
: > "$work/fetches"
if run > "$work/output" 2>&1; then fail '查不到最新版还装成功了'; fi
grep -Fq 'cdn.jsdelivr.net' "$work/output" || fail "查最新版失败时缺少提示：$(cat "$work/output")"
pass 'Connection failures without a mirror explain how to use one'

# macOS：下载 pkg，用系统的 installer 只装给当前用户，不碰 Linux 的包管理器
reset 'ID=unused'
test_system=Darwin
test_arch=arm64
result=$(run --print-plan) || fail 'macOS 计划失败'
[[ "$result" == *'WowType-0.3.8-macos.pkg'* ]] || fail "macOS 包选择错误：$result"
[[ ! -s "$work/commands" && ! -s "$work/fetches" ]] || fail 'macOS --print-plan 有副作用'
pass 'macOS plan picks the pkg'

reset 'ID=unused'
test_system=Darwin
asset WowType-0.3.8-macos.pkg
run > "$work/output" 2>&1 || fail "macOS install: $(cat "$work/output")"
assert_log 'installer -pkg'
assert_log 'WowType-0.3.8-macos.pkg -target CurrentUserHomeDirectory'
if grep -Eq '^(apt-get|pacman|dnf|wowtype-config) ' "$work/commands"; then fail 'macOS 跑了 Linux 的命令'; fi
pass 'macOS installs the verified pkg for the current user'

reset 'ID=unused'
test_system=Darwin
asset WowType-0.3.8-macos.pkg
printf 'corruption\n' >> "$work/assets/WowType-0.3.8-macos.pkg"
if run > "$work/output" 2>&1; then fail 'Corrupted macOS pkg accepted'; fi
assert_no_install
pass 'macOS checksum mismatch prevents install'

reset 'ID=unused'
test_system=Darwin
asset wowtype-0.3.8-1-x86_64.pkg.tar.zst
if run > "$work/output" 2>&1; then fail 'Release without macOS pkg accepted'; fi
grep -Fq 'macOS 安装包' "$work/output" || fail "缺少 macOS 提示：$(cat "$work/output")"
assert_no_install
pass 'Release without a macOS pkg fails clearly'

reset 'ID=unused'
test_system=Darwin
test_arch=ppc
if run --print-plan > "$work/output" 2>&1; then fail 'Unexpected PowerPC Mac'; fi
assert_no_install
pass 'macOS rejects unknown architectures'

printf '%s installer checks passed\n' "$count"
