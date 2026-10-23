#!/bin/bash
# Restores owned connection changes before removing this user's LaunchAgent.
set -euo pipefail
umask 077
[[ "$(uname -s)" == Darwin ]] || { printf '%s\n' '此卸载脚本仅支持 macOS。' >&2; exit 1; }
[[ "$#" == 0 ]] || { printf '%s\n' '用法: macos-uninstall.sh' >&2; exit 2; }

install_dir="$HOME/Library/Application Support/Pool Router"
binary="$install_dir/bin/pool-router"
config="$install_dir/config.json"
plist="$HOME/Library/LaunchAgents/local.pool-router.plist"
application="$HOME/Applications/Pool Router.app"
domain="gui/$(id -u)"

if [[ -e "$plist" ]]; then
  previous_binary="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$plist" 2>/dev/null || true)"
  [[ "$previous_binary" == "$binary" ]] || { printf '%s\n' '同名 LaunchAgent 属于其他安装，未卸载。' >&2; exit 1; }
fi

# A conflict must stop uninstall, otherwise Codex could be left pointing at a dead port.
if [[ -e "$install_dir/integration-backup.json" || -e "$install_dir/cc-monitor-backup.json" ]]; then
  [[ -x "$binary" ]] || { printf '%s\n' '存在待恢复接入，但缺少可执行文件。请先重新安装，再卸载。' >&2; exit 1; }
  "$binary" restore --config "$config" || {
    printf '%s\n' '接入恢复失败。为避免 Codex 失去入口，保留 Pool Router 服务和全部文件。' >&2
    exit 1
  }
fi

if /bin/launchctl print "$domain/local.pool-router" >/dev/null 2>&1; then
  [[ -f "$plist" ]] || { printf '%s\n' '存在未识别的同名运行服务，未停止。' >&2; exit 1; }
  /bin/launchctl bootout "$domain/local.pool-router"
fi
rm -f "$plist" "$binary"
rmdir "$install_dir/bin" 2>/dev/null || true
if [[ -e "$application" ]]; then
  app_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$application/Contents/Info.plist" 2>/dev/null || true)"
  if [[ "$app_id" == local.pool-router.launcher ]]; then
    rm -f "$application/Contents/MacOS/Pool Router" "$application/Contents/Info.plist"
    rmdir "$application/Contents/MacOS" "$application/Contents" "$application" 2>/dev/null || true
  fi
fi
printf '%s\n' '已恢复接入并卸载 Pool Router 服务和启动器。CC Switch 与 wiretap 保持运行。'
printf '%s\n' '配置、日志和私有恢复备份仍保存在 ~/Library/Application Support/Pool Router，可按需手动删除。'
