import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from aiohttp import ClientSession, web

from conversations import ConversationIndex
from wiretap import Journal, Proxy, WSObserver

A = '11111111-1111-4111-8111-111111111111'
B = '22222222-2222-4222-8222-222222222222'


class ConversationTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = Path(self.tmp.name)
        self.db = self.home / 'state_5.sqlite'
        with sqlite3.connect(self.db) as db:
            db.execute('CREATE TABLE threads (id TEXT PRIMARY KEY,title TEXT,cwd TEXT,first_user_message TEXT)')
            db.executemany('INSERT INTO threads VALUES (?,?,?,?)', [
                (A, '实验结果检查', '/private/work/project-a', 'BODY_MUST_NOT_BE_READ'),
                (B, '实验结果检查', '/private/work/project-b', 'BODY_MUST_NOT_BE_READ'),
            ])
        self.index = ConversationIndex(self.home, ttl=0)
        self.log = self.home / 'capture.jsonl'
        self.journal = Journal(self.log, self.index)

    def tearDown(self):
        self.journal.close()
        self.tmp.cleanup()

    def test_explicit_headers_and_metadata_use_exact_id(self):
        row = self.journal.new({'model':'example', 'client_metadata':{'thread_id':A,'secret':'DO_NOT_LOG'}}, 'HTTP', 'responses', {'Thread-ID':A,'Authorization':'SECRET'})
        c = self.index.enrich([row])[0]['conversation']
        self.assertEqual(c['id'], A)
        self.assertEqual(c['title'], '实验结果检查')
        self.assertEqual(c['project'], 'project-a')
        self.assertEqual(c['url'], 'codex://threads/'+A)
        log = self.log.read_text()
        for text in ['实验结果检查','project-a','BODY_MUST_NOT_BE_READ','DO_NOT_LOG','Authorization','/private/work']:
            self.assertNotIn(text, log)
        self.assertIn(A, log)

    def test_same_titles_never_merge_distinct_conversations(self):
        rows = [self.journal.new({},'HTTP','responses',{'Thread-Id':tid}) for tid in [A,B]]
        enriched = self.index.enrich(rows)
        self.assertEqual([r['conversation']['id'] for r in enriched], [A,B])
        self.assertNotEqual(enriched[0]['conversation']['url'], enriched[1]['conversation']['url'])

    def test_conflicting_ids_do_not_guess(self):
        c = self.index.identify({'client_metadata':{'thread_id':A}}, {'Thread-Id':B})
        self.assertIsNone(c['id'])
        self.assertEqual(c['status'], 'ambiguous')

    def test_arbitrary_cache_key_cannot_create_a_conversation(self):
        self.assertEqual(self.index.identify({'prompt_cache_key':A})['id'], A)
        unknown = '33333333-3333-4333-8333-333333333333'
        self.assertIsNone(self.index.identify({'prompt_cache_key':unknown})['id'])
        self.assertIsNone(self.index.identify({'prompt_cache_key':'sk-SECRET'})['id'])

    def test_missing_database_still_keeps_explicit_link(self):
        index = ConversationIndex(self.home / 'absent')
        c = index.identify({}, {'session-id':A})
        result = index.enrich([{'conversation':c}])[0]['conversation']
        self.assertEqual(result['status'], 'not_found')
        self.assertEqual(result['url'], 'codex://threads/'+A)
        self.assertIsNone(result['title'])

    def test_no_titles_option_and_legacy_records(self):
        index = ConversationIndex(self.home, titles=False)
        c = index.enrich([{'conversation':index.identify({}, {'Thread-Id':A})}])[0]['conversation']
        self.assertIsNone(c['title'])
        self.assertIsNone(c['project'])
        self.assertEqual(c['status'], 'local_match')
        self.assertEqual(index.enrich([{}])[0]['conversation']['status'], 'legacy')

    def test_malformed_or_hostile_ids_never_become_links(self):
        for value in ['javascript:alert(1)', A+'/../../etc/passwd', '<img src=x>', 123, None, '00000000-0000-0000-0000-000000000000']:
            self.assertIsNone(self.index.identify({}, {'Thread-Id':value})['id'])

    def test_schema_variations_and_title_rename(self):
        (self.home/'state_99.sqlite').write_bytes(b'not a database')
        self.assertEqual(self.index.lookup([A])[A]['title'], '实验结果检查')
        with sqlite3.connect(self.db) as db:
            db.execute('UPDATE threads SET title=? WHERE id=?', ('更名后的对话\n第二行',A))
        self.assertEqual(self.index.lookup([A])[A]['title'], '更名后的对话 第二行')

    def test_websocket_messages_keep_conversation_on_reuse(self):
        observer = WSObserver(self.journal,'responses',{'Session-Id':A})
        for i in range(2):
            observer.client({'type':'response.create','model':'example','client_metadata':{'thread_id':A}})
            observer.server({'type':'response.completed','response':{'id':f'resp_{i}','model':'example'}})
        self.assertTrue(all(r['conversation']['id']==A for r in self.journal.rows.values()))


class ConversationHTTPTest(unittest.IsolatedAsyncioTestCase):
    async def test_api_enriches_title_and_preserves_forwarded_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with sqlite3.connect(root/'state_5.sqlite') as db:
                db.execute('CREATE TABLE threads(id TEXT,title TEXT)')
                db.execute('INSERT INTO threads VALUES (?,?)',(A,'<script>example</script>'))
            journal = Journal(root/'capture.jsonl', ConversationIndex(root))
            runners = []
            async def start(app):
                runner = web.AppRunner(app,access_log=None); await runner.setup()
                site = web.TCPSite(runner,'127.0.0.1',0); await site.start(); runners.append(runner)
                return f'http://127.0.0.1:{site._server.sockets[0].getsockname()[1]}'
            async def upstream(request):
                self.assertEqual(request.headers['Thread-Id'],A)
                self.assertEqual((await request.json())['client_metadata']['thread_id'],A)
                return web.json_response({'model':'example'})
            app = web.Application(); app.router.add_post('/v1/responses',upstream)
            upstream_url = await start(app)
            proxy_url = await start(Proxy(upstream_url+'/v1',journal).app())
            try:
                async with ClientSession() as client:
                    async with client.post(proxy_url+'/v1/responses',headers={'Thread-Id':A},json={'model':'example','client_metadata':{'thread_id':A}}) as response:
                        self.assertEqual(response.status,200); await response.read()
                    async with client.get(proxy_url+'/__wiretap__/api/records') as response:
                        rows = (await response.json())['records']
                        self.assertEqual(rows[0]['conversation']['title'],'<script>example</script>')
                        self.assertEqual(rows[0]['conversation']['url'],'codex://threads/'+A)
                self.assertNotIn('<script>',journal.path.read_text())
            finally:
                for runner in reversed(runners): await runner.cleanup()
                journal.close()


if __name__ == '__main__':
    unittest.main()
