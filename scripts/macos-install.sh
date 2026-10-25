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

[[ -x /usr/bin/plutil ]] || { printf '%s\n' '缺少系统 plutil，无法安全生成 macOS plist。' >&2; exit 1; }
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
  if [[ ! -e "$application/Contents/Info.plist" ]]; then
    # An interrupted install leaves a bundle without Info.plist. Treat it as our
    # own leftover only while it holds nothing this script did not write.
    foreign="$(find "$application" -mindepth 1 \
      ! -path "$application/Contents" \
      ! -path "$application/Contents/MacOS" \
      ! -path "$application/Contents/MacOS/Pool Router" \
      ! -name .DS_Store -print -quit)"
    [[ -z "$foreign" ]] || { printf '%s\n' '同名应用不是 Pool Router 启动器，未覆盖。' >&2; exit 1; }
  else
    [[ "$previous_id" == local.pool-router.launcher ]] || { printf '%s\n' '同名应用不是 Pool Router 启动器，未覆盖。' >&2; exit 1; }
  fi
fi

if [[ "$build" == 1 ]]; then
  (cd "$crate_dir" && cargo build --release)
fi
[[ -x "$crate_dir/target/release/pool-router" ]] || { printf '%s\n' '未找到 release 可执行文件，请先执行 cargo build --release。' >&2; exit 1; }
version_output="$("$crate_dir/target/release/pool-router" --version)"
[[ "$version_output" == 'Pool Router '* ]] || { printf '%s\n' '无法识别待安装程序的版本。' >&2; exit 1; }
application_version="${version_output#Pool Router }"

mkdir -p "$install_dir/bin" "$install_dir/logs" "$agent_dir" "$application/Contents/MacOS"
chmod 700 "$install_dir" "$install_dir/bin" "$install_dir/logs"
# Atomic replacement leaves an already-running version intact until our own restart below.
temporary_binary="$(mktemp "$install_dir/bin/.pool-router.XXXXXX")"
temporary_plist=''
temporary_info=''
trap 'rm -f -- "$temporary_binary" "$temporary_plist" "$temporary_info"' EXIT
install -m 700 "$crate_dir/target/release/pool-router" "$temporary_binary"
mv -f "$temporary_binary" "$binary"
"$binary" init --config "$config"
install -m 700 "$script_dir/macos-uninstall.sh" "$install_dir/uninstall.sh"

# Generate each plist beside its destination, then replace it atomically.
# Passing values as separate arguments lets plutil escape paths safely.
temporary_plist="$(mktemp "$agent_dir/.pool-router.XXXXXX")"
/usr/bin/plutil -create xml1 -- "$temporary_plist"
/usr/bin/plutil -insert Label -string local.pool-router -- "$temporary_plist"
/usr/bin/plutil -insert ProgramArguments -xml '<array/>' -- "$temporary_plist"
/usr/bin/plutil -insert ProgramArguments.0 -string "$binary" -- "$temporary_plist"
/usr/bin/plutil -insert ProgramArguments.1 -string serve -- "$temporary_plist"
/usr/bin/plutil -insert ProgramArguments.2 -string --config -- "$temporary_plist"
/usr/bin/plutil -insert ProgramArguments.3 -string "$config" -- "$temporary_plist"
/usr/bin/plutil -insert WorkingDirectory -string "$install_dir" -- "$temporary_plist"
/usr/bin/plutil -insert RunAtLoad -bool true -- "$temporary_plist"
/usr/bin/plutil -insert KeepAlive -bool true -- "$temporary_plist"
/usr/bin/plutil -insert ThrottleInterval -integer 10 -- "$temporary_plist"
/usr/bin/plutil -insert Umask -integer 63 -- "$temporary_plist"
/usr/bin/plutil -insert StandardOutPath -string "$install_dir/logs/service.log" -- "$temporary_plist"
/usr/bin/plutil -insert StandardErrorPath -string "$install_dir/logs/service.log" -- "$temporary_plist"

temporary_info="$(mktemp "$application/Contents/.pool-router.XXXXXX")"
/usr/bin/plutil -create xml1 -- "$temporary_info"
/usr/bin/plutil -insert CFBundleIdentifier -string local.pool-router.launcher -- "$temporary_info"
/usr/bin/plutil -insert CFBundleName -string 'Pool Router' -- "$temporary_info"
/usr/bin/plutil -insert CFBundleDisplayName -string 'Pool Router' -- "$temporary_info"
/usr/bin/plutil -insert CFBundleExecutable -string 'Pool Router' -- "$temporary_info"
/usr/bin/plutil -insert CFBundlePackageType -string APPL -- "$temporary_info"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$application_version" -- "$temporary_info"
/usr/bin/plutil -insert LSUIElement -bool true -- "$temporary_info"
if [[ -f "$crate_dir/assets/PoolRouter.icns" ]]; then
  mkdir -p "$application/Contents/Resources"
  cp -f "$crate_dir/assets/PoolRouter.icns" "$application/Contents/Resources/PoolRouter.icns"
  chmod 644 "$application/Contents/Resources/PoolRouter.icns"
  /usr/bin/plutil -insert CFBundleIconFile -string PoolRouter -- "$temporary_info"
fi
/usr/bin/plutil -insert NSHighResolutionCapable -bool true -- "$temporary_info"
/usr/bin/plutil -lint -- "$temporary_plist" "$temporary_info" >/dev/null
chmod 600 "$temporary_plist" "$temporary_info"
mv -f "$temporary_plist" "$plist"
mv -f "$temporary_info" "$application/Contents/Info.plist"

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
touch "$application"


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
printf '%s\n' 'Pool Router 已安装。可双击 ~/Applications/Pool Router.app 打开面板。'
printf '%s\n' '安装仅启动本地路由；请在验证号池后通过面板或 connect 命令接入 Codex。'
printf '%s\n' '配置和恢复备份保存在 ~/Library/Application Support/Pool Router。'
if [[ "$show" == 1 ]]; then
  "$binary" open --config "$config" || printf '%s\n' '未能自动打开面板，可稍后双击 ~/Applications/Pool Router.app。' >&2
fi
