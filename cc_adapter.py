#!/usr/bin/env python3
"""CC Switch adapter: preserve original provider, install a supervised gateway."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import signal
import sqlite3
import subprocess
import sys
import time
import tomllib
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent
HOME = Path.home()
DB = HOME / '.cc-switch/cc-switch.db'
STATE = ROOT / 'data/cc-adapter.json'
LABEL = 'local.codex-wiretap'
PLIST = HOME / 'Library/LaunchAgents' / (LABEL + '.plist')
BASE = 'http://127.0.0.1:10812/v1'


def private_write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + '.' + uuid.uuid4().hex)
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'wb') as f:
        f.write(data)
    os.replace(tmp, path)


def replace_base(text, url):
    parsed = tomllib.loads(text)
    provider = parsed['model_provider']
    lines = text.splitlines(keepends=True)
    inside = False
    count = 0
    for i, line in enumerate(lines):
        if line.lstrip().startswith('['):
            inside = bool(re.match(r'^\s*\[model_providers\.(?:' + re.escape(provider) + '|' + re.escape(json.dumps(provider)) + r')\]\s*(?:#.*)?$', line.strip()))
        if inside and re.match(r'^\s*base_url\s*=', line):
            lines[i] = 'base_url = ' + json.dumps(url) + '\n'
            count += 1
    if count != 1:
        raise ValueError('配置结构不匹配，未修改文件')
    result = ''.join(lines)
    expected = json.loads(json.dumps(parsed))
    expected['model_providers'][provider]['base_url'] = url
    if tomllib.loads(result) != expected:
        raise ValueError('配置验证失败')
    return result


def prepare():
    if not DB.is_file():
        raise ValueError('未找到 CC Switch 数据库，请先安装并配置 CC Switch')
    with sqlite3.connect(DB) as db:
        db.row_factory = sqlite3.Row
        row = db.execute("SELECT * FROM providers WHERE app_type='codex' AND is_current=1").fetchone()
        if row is None:
            raise ValueError('未选择 Codex 供应商')
        meta = json.loads(row['meta'] or '{}')
        if meta.get('wiretapSourceProviderId'):
            row = db.execute("SELECT * FROM providers WHERE app_type='codex' AND id=?", (meta['wiretapSourceProviderId'],)).fetchone()
        if row is None:
            raise ValueError('原供应商已被删除，无法安装适配')
        source = dict(row)
        from wiretap import cc_provider
        upstream, _ = cc_provider(source['id'], DB)
        target_id = 'wiretap-' + source['id']
        name = source['name'] + '（本地抓包）'
        if not db.execute("SELECT 1 FROM providers WHERE app_type='codex' AND id=?", (target_id,)).fetchone():
            clone = dict(source)
            clone.update(id=target_id, name=name, is_current=0, in_failover_queue=0,
                         notes='常驻本地转发；在监测面板暂停记录即可，无需切换供应商。', created_at=int(time.time()*1000))
            data = json.loads(clone['settings_config'])
            data['config'] = replace_base(data['config'], BASE)
            clone['settings_config'] = json.dumps(data, ensure_ascii=False)
            meta = json.loads(clone['meta'] or '{}')
            meta.update(wiretapSourceProviderId=source['id'], endpointAutoSelect=False)
            clone['meta'] = json.dumps(meta, ensure_ascii=False)
            cols = list(clone)
            db.execute('INSERT INTO providers (' + ','.join('"'+k+'"' for k in cols) + ') VALUES (' + ','.join('?' for _ in cols) + ')', [clone[k] for k in cols])
        state = {'source_id': source['id'], 'source_name': source['name'], 'provider_id': target_id,
                 'provider_name': name, 'upstream': upstream, 'base_url': BASE}
    private_write(STATE, json.dumps(state, ensure_ascii=False, indent=2).encode())
    return state


def check_installation():
    if sys.platform != 'darwin':
        raise ValueError('CC Switch 自动安装仅支持 macOS；其他系统请使用独立代理模式')
    if not (ROOT / '.venv/bin/python').is_file():
        raise ValueError('请先按 README 创建 .venv 并安装依赖')
    if PLIST.exists():
        previous = plistlib.loads(PLIST.read_bytes())
        if str(ROOT / 'wiretap.py') not in previous.get('ProgramArguments', []):
            raise ValueError('其他目录已经安装同名服务，请先按 README 恢复原供应商并卸载旧服务')


def install_service(state):
    check_installation()
    definition = {'Label': LABEL, 'ProgramArguments': [str(ROOT / '.venv/bin/python'), str(ROOT / 'wiretap.py'),
                  '--cc-provider-id', state['source_id'], 'serve'], 'WorkingDirectory': str(ROOT),
                  'RunAtLoad': True, 'KeepAlive': True, 'ThrottleInterval': 10,
                  'StandardOutPath': str(ROOT / 'data/service.log'), 'StandardErrorPath': str(ROOT / 'data/service.log')}
    private_write(PLIST, plistlib.dumps(definition))
    domain = f'gui/{os.getuid()}'
    subprocess.run(['launchctl', 'bootout', domain + '/' + LABEL], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    old = subprocess.run(['lsof', '-t', '-iTCP:10812', '-sTCP:LISTEN'], text=True, capture_output=True)
    for pid in old.stdout.split():
        cmd = subprocess.check_output(['ps', '-p', pid, '-o', 'command='], text=True)
        if str(ROOT / 'wiretap.py') not in cmd:
            raise ValueError('10812 被其他程序占用')
        os.kill(int(pid), signal.SIGINT)
    deadline = time.monotonic() + 5
    while old.stdout and time.monotonic() < deadline:
        time.sleep(.1)
        old = subprocess.run(['lsof', '-t', '-iTCP:10812', '-sTCP:LISTEN'], text=True, capture_output=True)
    if old.stdout:
        raise ValueError('旧代理尚未退出，未启动第二个进程')
    subprocess.run(['launchctl', 'bootstrap', domain, str(PLIST)], check=True)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    for _ in range(30):
        try:
            with opener.open('http://127.0.0.1:10812/__wiretap__/api/records', timeout=1) as response:
                data = json.load(response)
            if data['upstream'] == state['upstream']:
                return
        except (OSError, ValueError):
            pass
        time.sleep(.2)
    raise ValueError('服务未就绪，供应商尚未切换')


def main():
    parser = argparse.ArgumentParser(description='安装 CC Switch 常驻抓包适配；原供应商保留')
    parser.add_argument('action', choices=['install', 'status'])
    args = parser.parse_args()
    if args.action == 'status':
        print(STATE.read_text())
        return
    check_installation()
    state = prepare()
    install_service(state)
    print(json.dumps({'provider': state['provider_name'], 'upstream': state['upstream'],
                      'service': LABEL, 'next_step': '在 CC Switch 中启用此供应商，必须通过应用切换才能更新运行状态。', 'dashboard': 'http://127.0.0.1:10812/__wiretap__/'}, ensure_ascii=False))


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        print('操作未完成：' + type(exc).__name__ + '。供应商和配置需按实际状态检查。')
        raise SystemExit(1)
