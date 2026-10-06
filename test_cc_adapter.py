import json
from pathlib import Path
import plistlib
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import cc_adapter
from wiretap import cc_provider, CCKeyLoader, ProviderCredentialsError


class AdapterTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.db = self.root / 'cc-switch.db'
        self.state = self.root / 'data/state.json'
        self.settings = {'auth': {'OPENAI_API_KEY': 'SYNTHETIC_TEST_KEY'}, 'config': 'model_provider="custom"\n[model_providers.custom]\nbase_url="https://provider.example/v1"\nrequires_openai_auth=true\n'}
        with sqlite3.connect(self.db) as db:
            db.execute('CREATE TABLE providers (id TEXT, app_type TEXT, name TEXT, settings_config TEXT, meta TEXT, is_current INTEGER, in_failover_queue INTEGER, notes TEXT, created_at INTEGER, PRIMARY KEY(id, app_type))')
            db.execute('INSERT INTO providers VALUES (?,?,?,?,?,?,?,?,?)', ('original', 'codex', 'Example', json.dumps(self.settings), '{}', 1, 0, '', 0))

    def tearDown(self):
        self.tmp.cleanup()

    def prepare(self):
        with patch.object(cc_adapter, 'DB', self.db), patch.object(cc_adapter, 'STATE', self.state):
            return cc_adapter.prepare()

    def test_clone_preserves_original_and_does_not_activate_itself(self):
        before = self.settings.copy()
        result = self.prepare()
        with sqlite3.connect(self.db) as db:
            original = db.execute('SELECT settings_config,is_current FROM providers WHERE id=?', ('original',)).fetchone()
            clone = db.execute('SELECT settings_config,is_current,meta FROM providers WHERE id=?', (result['provider_id'],)).fetchone()
        self.assertEqual(json.loads(original[0]), before)
        self.assertEqual(original[1], 1)
        self.assertEqual(clone[1], 0)
        self.assertIn('http://127.0.0.1:10812/v1', json.loads(clone[0])['config'])
        self.assertEqual(json.loads(clone[2])['wiretapSourceProviderId'], 'original')
        self.assertNotIn('SYNTHETIC_TEST_KEY', self.state.read_text())
        self.assertEqual(self.state.stat().st_mode & 0o777, 0o600)

    def test_prepare_is_idempotent_even_when_clone_is_selected(self):
        result = self.prepare()
        with sqlite3.connect(self.db) as db:
            db.execute('UPDATE providers SET is_current=(id=?)', (result['provider_id'],))
        again = self.prepare()
        self.assertEqual(again['source_id'], 'original')
        with sqlite3.connect(self.db) as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM providers').fetchone()[0], 2)

    def test_clone_edit_can_discard_metadata_without_losing_source_link(self):
        result = self.prepare()
        with sqlite3.connect(self.db) as db:
            db.execute('UPDATE providers SET is_current=(id=?)', (result['provider_id'],))
            db.execute('UPDATE providers SET meta=? WHERE id=?', ('{}', result['provider_id']))
        self.assertEqual(self.prepare()['source_id'], 'original')

    def test_key_rotates_without_restart_but_does_not_follow_new_destination(self):
        loader = CCKeyLoader('original', 'https://provider.example/v1', self.db)
        self.assertEqual(loader(), 'SYNTHETIC_TEST_KEY')
        changed = json.loads(json.dumps(self.settings))
        changed['auth']['OPENAI_API_KEY'] = 'ROTATED_TEST_KEY'
        with sqlite3.connect(self.db) as db:
            db.execute('UPDATE providers SET settings_config=? WHERE id=?', (json.dumps(changed), 'original'))
        self.assertEqual(loader(), 'ROTATED_TEST_KEY')
        changed['config'] = changed['config'].replace('provider.example', 'different.example')
        with sqlite3.connect(self.db) as db:
            db.execute('UPDATE providers SET settings_config=? WHERE id=?', (json.dumps(changed), 'original'))
        with self.assertRaises(ProviderCredentialsError):
            loader()

    def test_missing_key_fails_without_reusing_cached_secret(self):
        loader = CCKeyLoader('original', 'https://provider.example/v1', self.db)
        self.assertTrue(loader())
        changed = json.loads(json.dumps(self.settings))
        changed['auth'].clear()
        with sqlite3.connect(self.db) as db:
            db.execute('UPDATE providers SET settings_config=? WHERE id=?', (json.dumps(changed), 'original'))
        with self.assertRaises(ProviderCredentialsError):
            loader()

    def test_missing_database_is_not_created(self):
        absent = self.root / 'missing.db'
        with patch.object(cc_adapter, 'DB', absent), self.assertRaises(ValueError):
            cc_adapter.prepare()
        self.assertFalse(absent.exists())

    def test_key_loading_rejects_url_credentials_before_use(self):
        for url in ['http://provider.example/v1', 'https://user:pass@provider.example/v1', 'https://provider.example/v1?key=secret']:
            settings = dict(self.settings)
            settings['config'] = settings['config'].replace('https://provider.example/v1', url)
            with sqlite3.connect(self.db) as db:
                db.execute('UPDATE providers SET settings_config=?', (json.dumps(settings),))
            with self.assertRaises(ValueError):
                cc_provider('original', self.db)

    def test_install_check_rejects_other_checkout_without_stopping_it(self):
        plist = self.root / 'service.plist'
        (self.root / '.venv/bin').mkdir(parents=True)
        (self.root / '.venv/bin/python').touch()
        plist.write_bytes(plistlib.dumps({'ProgramArguments': ['/different/checkout/wiretap.py']}))
        with patch.object(cc_adapter, 'ROOT', self.root), patch.object(cc_adapter, 'PLIST', plist), patch('cc_adapter.sys.platform', 'darwin'), self.assertRaises(ValueError):
            cc_adapter.check_installation()
        self.assertTrue(plist.exists())


if __name__ == '__main__':
    unittest.main()
