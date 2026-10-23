"""Correlate explicit wire IDs with local Codex titles; never read message bodies."""
from __future__ import annotations

import os
from contextlib import closing
from pathlib import Path
import re
import sqlite3
import time

UUID = re.compile(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\Z")


def thread_id(value):
    if isinstance(value, str) and UUID.fullmatch(value) and value != '00000000-0000-0000-0000-000000000000':
        return value.lower()
    return None


def mapping(value):
    return value if isinstance(value, dict) else {}


def clean_title(value):
    if not isinstance(value, str):
        return None
    # Remove controls, bidi overrides and long prompt-like titles from display.
    value = re.sub(r'[\x00-\x1f\x7f\u202a-\u202e\u2066-\u2069]', ' ', value)
    return ' '.join(value.split())[:160] or None


class ConversationIndex:
    def __init__(self, home=None, titles=True, ttl=5):
        self.home = Path(home or os.environ.get('CODEX_HOME', str(Path.home() / '.codex')))
        self.titles = titles
        self.ttl = ttl
        self.cache = {}

    def lookup(self, ids):
        now = time.monotonic()
        ids = {tid for value in ids if (tid := thread_id(value))}
        missing = [tid for tid in ids if tid not in self.cache or now - self.cache[tid][0] >= self.ttl]
        found = {}
        if missing:
            # Newer schemas first; query only the exact requested IDs, not all chats.
            try:
                databases = sorted(self.home.glob('state_*.sqlite'), key=lambda p: int(m.group(1)) if (m := re.fullmatch(r'state_(\d+)\.sqlite', p.name)) else -1, reverse=True)
            except OSError:
                databases = []
            for path in databases:
                pending = [tid for tid in missing if tid not in found]
                if not pending:
                    break
                try:
                    with closing(sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True, timeout=.05)) as db:
                        db.execute('PRAGMA query_only=ON')
                        columns = {row[1] for row in db.execute('PRAGMA table_info(threads)')}
                        if 'id' not in columns:
                            continue
                        fields = ['id'] + ([c for c in ('title', 'name', 'cwd') if c in columns] if self.titles else [])
                        for start in range(0, len(pending), 300):
                            batch = pending[start:start+300]
                            sql = 'SELECT ' + ','.join('"id"' if c == 'id' else 'substr("'+c+'",1,500)' for c in fields) + ' FROM threads WHERE id IN (' + ','.join('?' for _ in batch) + ')'
                            for row in db.execute(sql, batch):
                                data = dict(zip(fields, row))
                                title = clean_title(data.get('title')) or clean_title(data.get('name'))
                                cwd = data.get('cwd')
                                project = clean_title(re.split(r'[/\\]', cwd.rstrip('/\\'))[-1]) if isinstance(cwd, str) and cwd else None
                                found[data['id']] = {'title': title, 'project': project}
                except (sqlite3.Error, OSError, ValueError):
                    # Missing, locked or changed local state never blocks forwarding.
                    continue
            for tid in missing:
                self.cache[tid] = (now, found.get(tid))
            if len(self.cache) > 1000:
                self.cache = {k: v for k, v in self.cache.items() if k in ids}
        return {tid: self.cache[tid][1] for tid in ids if tid in self.cache and self.cache[tid][1] is not None}

    def identify(self, data, headers=None):
        data = mapping(data)
        body = mapping(data.get('response')) or data
        meta = mapping(body.get('client_metadata'))
        headers = {k.lower(): v for k, v in (headers or {}).items()}
        groups = [
            [('client_metadata.thread_id', meta.get('thread_id')), ('header.thread-id', headers.get('thread-id'))],
            [('client_metadata.session_id', meta.get('session_id')), ('header.session-id', headers.get('session-id'))],
        ]
        for group in groups:
            candidates = [(source, tid) for source, value in group if (tid := thread_id(value))]
            unique = {tid for _, tid in candidates}
            if len(unique) > 1:
                return {'id': None, 'status': 'ambiguous', 'source': 'conflicting_thread_ids'}
            if candidates:
                source, tid = candidates[0]
                return {'id': tid, 'status': 'reported', 'source': source}
        # prompt_cache_key is not inherently a thread ID. Accept only exact local matches.
        candidate = thread_id(body.get('prompt_cache_key'))
        if candidate and candidate in self.lookup([candidate]):
            return {'id': candidate, 'status': 'reported', 'source': 'prompt_cache_key_local_match'}
        return {'id': None, 'status': 'unidentified', 'source': None}

    def enrich(self, records):
        local = self.lookup(mapping(row.get('conversation')).get('id') for row in records)
        result = []
        for row in records:
            record = {k: v for k, v in row.items() if not k.startswith('_')}
            saved = mapping(row.get('conversation'))
            tid = thread_id(saved.get('id'))
            conversation = {'id': tid, 'source': saved.get('source'), 'status': saved.get('status', 'legacy'),
                            'title': None, 'project': None, 'url': None}
            if tid:
                conversation['url'] = 'codex://threads/' + tid
                conversation['status'] = 'local_match' if tid in local else 'not_found'
                if tid in local:
                    conversation.update(local[tid])
            record['conversation'] = conversation
            result.append(record)
        return result
