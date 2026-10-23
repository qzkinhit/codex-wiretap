#!/bin/bash
# Installs this crate's release binary and a user LaunchAgent. Does not connect Codex.
set -euo pipefail
umask 077

if [[ "$(uname -s)" != Darwin ]]; then
  printf '%s\n' '此安装脚本仅支持 macOS。' >&2
  exit 1
fi

build=1
start=1
show=1
for argument in "$@"; do
  case "$argument" in
    --no-build) build=0 ;;
    --no-start) start=0; show=0 ;;
    --no-open) show=0 ;;
    *) printf '%s\n' '用法: macos-install.sh [--no-build] [--no-start] [--no-open]' >&2; exit 2 ;;
  esac
done

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$(cd -- "$script_dir/.." && pwd)"
install_dir="$HOME/Library/Application Support/Pool Router"
binary="$install_dir/bin/pool-router"
config="$install_dir/config.json"
agent_dir="$HOME/Library/LaunchAgents"
plist="$agent_dir/local.pool-router.plist"
application="$HOME/Applications/Pool Router.app"
domain="gui/$(id -u)"

command -v python3 >/dev/null || { printf '%s\n' '缺少 python3，无法安全生成 macOS plist。' >&2; exit 1; }
if [[ ! -e "$plist" ]] && /bin/launchctl print "$domain/local.pool-router" >/dev/null 2>&1; then
  printf '%s\n' '存在未识别的同名运行服务，未安装或停止它。' >&2
  exit 1
fi
if [[ -e "$plist" ]]; then
  previous_binary="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$plist" 2>/dev/null || true)"
  [[ "$previous_binary" == "$binary" ]] || { printf '%s\n' '同名 LaunchAgent 属于其他安装，未覆盖。' >&2; exit 1; }
fi
if [[ -e "$application" ]]; then
  previous_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$application/Contents/Info.plist" 2>/dev/null || true)"
  [[ "$previous_id" == local.pool-router.launcher ]] || { printf '%s\n' '同名应用不是 Pool Router 启动器，未覆盖。' >&2; exit 1; }
fi

if [[ "$build" == 1 ]]; then
  (cd "$crate_dir" && cargo build --release)
fi
[[ -x "$crate_dir/target/release/pool-router" ]] || { printf '%s\n' '未找到 release 可执行文件，请先执行 cargo build --release。' >&2; exit 1; }

mkdir -p "$install_dir/bin" "$install_dir/logs" "$agent_dir" "$application/Contents/MacOS"
chmod 700 "$install_dir" "$install_dir/bin" "$install_dir/logs"
# Atomic replacement leaves an already-running version intact until our own restart below.
temporary_binary="$(mktemp "$install_dir/bin/.pool-router.XXXXXX")"
trap 'rm -f "$temporary_binary"' EXIT
install -m 700 "$crate_dir/target/release/pool-router" "$temporary_binary"
mv -f "$temporary_binary" "$binary"
"$binary" init --config "$config"
install -m 700 "$script_dir/macos-uninstall.sh" "$install_dir/uninstall.sh"

python3 - "$install_dir" "$plist" "$application" <<'PY'
import os, plistlib, sys, tempfile
from pathlib import Path
root, agent, app = map(Path, sys.argv[1:])
def write_plist(path, content):
    descriptor, temporary = tempfile.mkstemp(prefix='.pool-router.', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            os.fchmod(stream.fileno(), 0o600)
            plistlib.dump(content, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
write_plist(agent, {
    'Label': 'local.pool-router',
    'ProgramArguments': [str(root/'bin/pool-router'), 'serve', '--config', str(root/'config.json')],
    'WorkingDirectory': str(root),
    'RunAtLoad': True,
    'KeepAlive': True,
    'ThrottleInterval': 10,
    'Umask': 0o077,
    'StandardOutPath': str(root/'logs/service.log'),
    'StandardErrorPath': str(root/'logs/service.log'),
})
write_plist(app/'Contents/Info.plist', {
    'CFBundleIdentifier': 'local.pool-router.launcher',
    'CFBundleName': 'Pool Router',
    'CFBundleDisplayName': 'Pool Router',
    'CFBundleExecutable': 'Pool Router',
    'CFBundlePackageType': 'APPL',
    'CFBundleShortVersionString': '0.1.0',
    'LSUIElement': True,
    'NSHighResolutionCapable': True,
})
PY

cat > "$application/Contents/MacOS/Pool Router" <<'APP'
#!/bin/bash
set -euo pipefail
root="$HOME/Library/Application Support/Pool Router"
domain="gui/$(id -u)"
if ! /bin/launchctl print "$domain/local.pool-router" >/dev/null 2>&1; then
  /bin/launchctl bootstrap "$domain" "$HOME/Library/LaunchAgents/local.pool-router.plist"
fi
# The CLI reads the private token itself and opens a loopback URL fragment.
# It must never print that token to application or service logs.
exec "$root/bin/pool-router" open --config "$root/config.json"
APP
chmod 700 "$application/Contents/MacOS/Pool Router"
/usr/bin/plutil -lint "$plist" "$application/Contents/Info.plist" >/dev/null

if [[ "$start" == 1 ]]; then
  if /bin/launchctl print "$domain/local.pool-router" >/dev/null 2>&1; then
    # The executable/config paths are stable. Keeping the registered job avoids
    # a bootout/bootstrap race and an interrupted update leaving no background job.
    /bin/launchctl kickstart -k "$domain/local.pool-router"
  else
    loaded=0
    for attempt in 1 2 3; do
      if /bin/launchctl bootstrap "$domain" "$plist"; then
        loaded=1
        break
      fi
      if /bin/launchctl print "$domain/local.pool-router" >/dev/null 2>&1; then
        /bin/launchctl kickstart "$domain/local.pool-router"
        loaded=1
        break
      fi
      /bin/sleep 1
    done
    [[ "$loaded" == 1 ]] || { printf '%s\n' '后台服务尚未注册成功，可重新运行安装脚本；配置和恢复备份已保留。' >&2; exit 1; }
  fi
fi
if [[ "$show" == 1 ]]; then
  "$binary" open --config "$config"
fi
printf '%s\n' 'Pool Router 已安装。可双击 ~/Applications/Pool Router.app 打开面板。'
printf '%s\n' '安装仅启动本地路由；请在验证号池后通过面板或 connect 命令接入 Codex。'
printf '%s\n' '配置和恢复备份保存在 ~/Library/Application Support/Pool Router。'
