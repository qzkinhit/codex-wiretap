import asyncio
import gzip
import json
from pathlib import Path
import tempfile
import unittest

from aiohttp import web, ClientSession
from yarl import URL
from wiretap import Journal, LIMIT, Observer, Proxy, SSE, WSObserver, configuration, connection_status, read_report, request_fields, zstd, ProviderCredentialsError


class FieldsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.log = Path(self.temp.name) / 'capture.jsonl'
        self.journal = Journal(self.log)

    def tearDown(self):
        self.journal.close()
        self.temp.cleanup()

    def test_missing_is_not_none_effort_and_zero_tokens_preserved(self):
        row = self.journal.new({'model': 'gpt-example', 'reasoning': {'effort': 'none'}}, 'HTTP', 'responses')
        self.journal.observe(row, {'model': 'gpt-example', 'usage': {'output_tokens_details': {'reasoning_tokens': 0}}})
        self.assertEqual(row['effort_comparison'], 'unknown')
        self.assertEqual(row['model_comparison'], 'same')
        self.assertEqual(row['response']['reasoning_tokens'], 0)
        self.assertEqual(row['request']['reasoning_effort'], 'none')

    def test_whitelist_does_not_store_secrets_or_content(self):
        row = self.journal.new({'model': 'gpt-example', 'input': 'secret prompt', 'api_key': 'SECRET', 'authorization': 'Bearer SECRET'}, 'HTTP', 'responses')
        self.journal.observe(row, {'response': {'model': 'gpt-other', 'output': 'secret response', 'error': {'message': 'SECRET', 'code': 'invalid_model'}}})
        text = self.log.read_text()
        for secret in ['SECRET', 'secret prompt', 'secret response', 'authorization', 'api_key']:
            self.assertNotIn(secret, text)
        self.assertEqual(row['model_comparison'], 'different')
        self.assertEqual(self.log.stat().st_mode & 0o777, 0o600)

    def test_sse_arbitrary_byte_boundaries_and_multiline(self):
        stream = (': comment\r\ndata: {"type":"response.completed",\r\n'
                  'data: "response":{"model":"gpt-example","output":"中文",'
                  '"reasoning":{"effort":"high"}}}\r\n\r\ndata: [DONE]\n\n').encode()
        for size in [1, 2, 7, 49, len(stream)]:
            output = []
            parser = SSE(output.append)
            for pos in range(0, len(stream), size):
                parser.feed(stream[pos:pos+size])
            parser.finish()
            self.assertEqual(len(output), 1)
            self.assertEqual(output[0]['response']['output'], '中文')
            self.assertFalse(parser.truncated)

    def test_sse_skips_oversized_event_then_recovers(self):
        output = []
        parser = SSE(output.append)
        parser.feed(b'data: ' + b'x' * (LIMIT + 1) + b'\ndata: {"model":"should-not-appear"}\n\n')
        parser.feed(b'data: {"model":"next"}\n\n')
        self.assertEqual(output, [{'model': 'next'}])
        self.assertTrue(parser.truncated)

    def test_sse_does_not_invent_completed_truncated_event(self):
        output = []
        parser = SSE(output.append)
        parser.feed(b'data: {"model":"x"}')
        parser.finish()
        self.assertEqual(output, [])
        self.assertTrue(parser.truncated)

    def test_compressed_response_and_snapshot_report(self):
        row = self.journal.new({'model': 'x'}, 'SSE', 'responses')
        observer = Observer(self.journal, row, 'text/event-stream', 'gzip')
        payload = gzip.compress(b'data: {"response":{"model":"x","reasoning":{"effort":"high"}}}\n\n')
        for pos in range(0, len(payload), 3):
            observer.feed(payload[pos:pos+3])
        observer.finish()
        self.journal.finish(row)
        self.assertEqual(row['response']['reasoning_effort'], 'high')
        self.assertEqual(len(read_report(self.log)), 1)

    def test_zstd_concatenated_frames(self):
        row = self.journal.new({'model':'x'}, 'SSE', 'responses')
        observer = Observer(self.journal, row, 'text/event-stream', 'zstd')
        data = zstd.compress(b'data: {"response":{"model":"x"}}\n\n') + zstd.compress(b'data: {"response":{"reasoning":{"effort":"ultra"}}}\n\n')
        for offset in range(0, len(data), 3):
            observer.feed(data[offset:offset+3])
        observer.finish()
        self.assertEqual(row['response']['reasoning_effort'], 'ultra')

    def test_ws_reused_connection_and_ambiguous_pairing(self):
        observer = WSObserver(self.journal, 'responses')
        for i in range(2):
            observer.client({'type': 'response.create', 'model': f'm{i}', 'reasoning': {'effort': 'high'}})
            observer.server({'type': 'response.created', 'response': {'id': f'r{i}', 'model': f'm{i}'}})
            observer.server({'type': 'response.completed', 'response': {'id': f'r{i}', 'usage': {'output_tokens_details': {'reasoning_tokens': i}}}})
        self.assertEqual(len(self.journal.rows), 2)
        self.assertTrue(all(r['model_comparison'] == 'same' for r in self.journal.rows.values()))
        observer.client({'type': 'response.create', 'model': 'a'})
        observer.client({'type': 'response.create', 'model': 'b'})
        observer.server({'type': 'response.created', 'response': {'id': 'ambiguous', 'model': 'b'}})
        self.assertEqual(self.journal.rows[next(reversed(self.journal.rows))]['model_comparison'], 'unknown')
        observer.close()

    def test_chat_completions_and_config_profile(self):
        self.assertEqual(request_fields({'reasoning_effort': 'ultra'})['reasoning_effort'], 'ultra')
        config = Path(self.temp.name) / 'config.toml'
        config.write_text('model="base"\nmodel_provider="custom"\n[model_providers.custom]\nbase_url="http://127.0.0.1:10000/v1"\n[profiles.test]\nmodel="profile-model"\nmodel_reasoning_effort="ultra"\n')
        result = configuration(config, 'test')
        self.assertEqual(result['model'], 'profile-model')
        self.assertEqual(result['reasoning_effort'], 'ultra')
        (config.parent / 'test.config.toml').write_text('model="layered-model"\n[model_providers.custom]\nbase_url="http://127.0.0.1:10001/v1"\n')
        result = configuration(config, 'test')
        self.assertEqual(result['model'], 'layered-model')
        self.assertEqual(result['upstream'], 'http://127.0.0.1:10001/v1')

    def test_connection_status_detects_config_rewrite_without_disclosing_keys(self):
        config = Path(self.temp.name) / 'config.toml'
        template = 'model_provider="custom"\n[model_providers.custom]\nbase_url="http://127.0.0.1:{port}/v1"\napi_key="SECRET"\n'
        config.write_text(template.format(port=10812))
        self.assertTrue(connection_status(config, 'http://127.0.0.1:10812/v1')['configured'])
        config.write_text(template.format(port=10811))
        state = connection_status(config, 'http://127.0.0.1:10812/v1')
        self.assertFalse(state['configured'])
        self.assertNotIn('SECRET', json.dumps(state))
        config.write_text('not valid TOML')
        self.assertIsNone(connection_status(config, 'http://127.0.0.1:10812/v1')['configured'])

    def test_pause_persists_and_only_new_requests_are_excluded(self):
        old = self.journal.new({'model':'before'}, 'HTTP', 'responses')
        self.journal.set_enabled(False)
        paused = self.journal.new({'model':'during'}, 'HTTP', 'responses')
        self.journal.observe(paused, {'model':'during'})
        self.journal.finish(paused)
        self.journal.observe(old, {'model':'before'})
        self.journal.finish(old)
        self.assertEqual(len(self.journal.rows), 1)
        self.assertNotIn('during', self.log.read_text())
        other = Journal(self.log)
        self.assertFalse(other.enabled)
        other.close()
        self.journal.set_enabled(True)
        self.journal.new({'model':'after'}, 'HTTP', 'responses')
        self.assertEqual(len(self.journal.rows), 2)

    def test_cc_base_replacement_preserves_auth_and_other_settings(self):
        from cc_adapter import replace_base
        text = 'model_provider="custom"\nmodel="model1"\n[model_providers.custom]\nbase_url="https://example.com/v1"\nrequires_openai_auth=true\n'
        updated = replace_base(text, 'http://127.0.0.1:10812/v1')
        self.assertIn('requires_openai_auth=true', updated)
        self.assertIn('http://127.0.0.1:10812/v1', updated)


class IntegrationTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.journal = Journal(Path(self.temp.name) / 'capture.jsonl')
        self.runners = []
        self.client = ClientSession(auto_decompress=False)

    async def asyncTearDown(self):
        await self.client.close()
        for runner in reversed(self.runners):
            await runner.cleanup()
        self.journal.close()
        self.temp.cleanup()

    async def start(self, app):
        runner = web.AppRunner(app, access_log=None, auto_decompress=False, handler_cancellation=True)
        await runner.setup()
        site = web.TCPSite(runner, '127.0.0.1', 0)
        await site.start()
        self.runners.append(runner)
        return f'http://127.0.0.1:{site._server.sockets[0].getsockname()[1]}'

    async def setup_proxy(self, handler):
        app = web.Application()
        app.router.add_route('*', '/{tail:.*}', handler)
        upstream = await self.start(app)
        proxy = await self.start(Proxy(upstream + '/v1', self.journal).app())
        return proxy

    async def test_json_body_auth_query_and_response_preserved(self):
        payload = b'{ "model": "example", "reasoning": {"effort": "high"}, "input": "SECRET" }'
        response = b'{"model":"example-version","reasoning":{"effort":"low"},"usage":{"output_tokens_details":{"reasoning_tokens":23}}}'

        async def handler(request):
            self.assertEqual(await request.read(), payload)
            self.assertEqual(request.headers['Authorization'], 'Bearer SECRET')
            self.assertEqual(request.raw_path, '/v1/responses?value=a%2Fb')
            return web.Response(body=response, content_type='application/json', headers={'X-Request-ID':'123'})

        base = await self.setup_proxy(handler)
        async with self.client.post(URL(base + '/v1/responses?value=a%2Fb', encoded=True), data=payload, headers={'Authorization':'Bearer SECRET','Content-Type':'application/json'}) as res:
            self.assertEqual(res.status, 200)
            self.assertEqual(res.headers['X-Request-ID'], '123')
            self.assertEqual(await res.read(), response)
        row = list(self.journal.rows.values())[0]
        self.assertEqual(row['model_comparison'], 'different')
        self.assertEqual(row['effort_comparison'], 'different')
        self.assertEqual(row['response']['reasoning_tokens'], 23)
        self.assertNotIn('SECRET', self.journal.path.read_text())

    async def test_stream_delivers_before_upstream_finishes(self):
        release = asyncio.Event()
        first = b'data: {"type":"response.created","response":{"id":"r1","model":"x"}}\n\n'
        last = b'data: {"type":"response.completed","response":{"id":"r1","usage":{"output_tokens_details":{"reasoning_tokens":0}}}}\n\n'

        async def handler(request):
            response = web.StreamResponse(headers={'Content-Type':'text/event-stream'})
            await response.prepare(request)
            await response.write(first)
            await release.wait()
            await response.write(last)
            return response

        base = await self.setup_proxy(handler)
        async with self.client.post(base + '/v1/responses', json={'model':'x'}) as res:
            try:
                self.assertEqual(await asyncio.wait_for(res.content.readexactly(len(first)), 2), first)
            finally:
                release.set()
            self.assertEqual(await res.read(), last)
        row = list(self.journal.rows.values())[0]
        self.assertEqual(row['response']['reasoning_tokens'], 0)
        self.assertEqual(row['phase'], 'finished')

    async def test_client_closes_after_completed_is_not_failure(self):
        end = b'data: {"type":"response.completed","response":{"id":"r1","model":"x","status":"completed"}}\n\n'
        async def handler(request):
            response = web.StreamResponse(headers={'Content-Type':'text/event-stream'})
            await response.prepare(request)
            await response.write(end)
            await asyncio.Event().wait()
        base = await self.setup_proxy(handler)
        async with self.client.post(base + '/v1/responses', json={'model':'x'}) as res:
            self.assertEqual(await res.content.readexactly(len(end)), end)
        for _ in range(50):
            if list(self.journal.rows.values())[0]['phase'] == 'finished':
                break
            await asyncio.sleep(.01)
        row = list(self.journal.rows.values())[0]
        self.assertEqual(row['phase'], 'finished')
        self.assertNotIn('client_disconnected', row['notes'])

    async def test_zstd_request_forwarded_without_decompression(self):
        data = zstd.compress(b'{"model":"zstd-model","reasoning":{"effort":"high"}}')
        async def handler(request):
            self.assertEqual(await request.read(), data)
            self.assertEqual(request.headers['Content-Encoding'], 'zstd')
            return web.json_response({'model':'zstd-model'})
        base = await self.setup_proxy(handler)
        async with self.client.post(base + '/v1/responses', data=data, headers={'Content-Encoding':'zstd'}) as res:
            self.assertEqual(res.status, 200)
            await res.read()
        row = list(self.journal.rows.values())[0]
        self.assertEqual(row['request']['reasoning_effort'], 'high')
        self.assertEqual(row['model_comparison'], 'same')

    async def test_pause_control_keeps_forwarding_and_blocks_cross_site(self):
        calls = []
        async def handler(request):
            calls.append(await request.json())
            return web.json_response({'model':'ok'})
        base = await self.setup_proxy(handler)
        control = base + '/__wiretap__/api/recording'
        async with self.client.post(control, json={'enabled':False}) as r:
            self.assertEqual(r.status, 403)
        headers = {'X-Wiretap-Control':'1'}
        async with self.client.post(control, json={'enabled':False}, headers=headers) as r:
            self.assertTrue((await r.json())['forwarding'])
        async with self.client.post(base+'/v1/responses', json={'model':'paused'}) as r:
            self.assertEqual(r.status, 200)
            await r.read()
        self.assertEqual(len(self.journal.rows), 0)
        async with self.client.post(control, json={'enabled':True}, headers=headers) as r:
            await r.read()
        async with self.client.post(base+'/v1/responses', json={'model':'active'}) as r:
            await r.read()
        self.assertEqual(len(calls), 2)
        self.assertEqual(len(self.journal.rows), 1)

    async def test_cc_key_replaces_login_token_only_on_fixed_upstream(self):
        async def handler(request):
            self.assertEqual(request.headers['Authorization'], 'Bearer PROVIDER_KEY')
            self.assertNotIn('ChatGPT-Account-ID', request.headers)
            return web.json_response({'model':'ok'})
        app = web.Application(); app.router.add_post('/v1/responses', handler)
        upstream = await self.start(app)
        base = await self.start(Proxy(upstream+'/v1', self.journal, api_key='PROVIDER_KEY').app())
        async with self.client.post(base+'/v1/responses', json={'model':'x'}, headers={'Authorization':'Bearer LOGIN_TOKEN','ChatGPT-Account-ID':'PRIVATE'}) as r:
            self.assertEqual(r.status, 200)
            await r.read()
        self.assertNotIn('PROVIDER_KEY', self.journal.path.read_text())
        self.assertNotIn('LOGIN_TOKEN', self.journal.path.read_text())

    async def test_key_loader_is_called_for_each_http_request(self):
        observed = []
        async def handler(request):
            observed.append(request.headers.get('Authorization'))
            return web.json_response({'model':'ok'})
        app = web.Application(); app.router.add_post('/v1/responses',handler)
        upstream = await self.start(app)
        keys = iter(['FIRST_KEY','SECOND_KEY'])
        base = await self.start(Proxy(upstream+'/v1',self.journal,key_loader=lambda:next(keys)).app())
        for _ in range(2):
            async with self.client.post(base+'/v1/responses',json={'model':'x'}) as r:
                self.assertEqual(r.status,200); await r.read()
        self.assertEqual(observed,['Bearer FIRST_KEY','Bearer SECOND_KEY'])
        self.assertNotIn('SECOND_KEY',self.journal.path.read_text())

    async def test_failed_key_loading_never_falls_back_to_login_token(self):
        observed = []
        async def handler(request):
            observed.append(True); return web.Response()
        def fail():
            raise ProviderCredentialsError()
        app = web.Application(); app.router.add_post('/v1/responses',handler)
        upstream = await self.start(app)
        base = await self.start(Proxy(upstream+'/v1',self.journal,key_loader=fail).app())
        async with self.client.post(base+'/v1/responses',json={'model':'x'},headers={'Authorization':'Bearer CLIENT_KEY'}) as r:
            self.assertEqual(r.status,502)
            self.assertEqual((await r.json())['error']['type'],'wiretap_credentials_error')
        self.assertEqual(observed,[])

    async def test_websocket_reuse_frames_and_token_attribution(self):
        async def handler(request):
            self.assertEqual(request.headers['Authorization'], 'Bearer SECRET')
            ws = web.WebSocketResponse()
            await ws.prepare(request)
            async for message in ws:
                data = json.loads(message.data)
                i = data['index']
                await ws.send_json({'type':'response.created','response':{'id':f'r{i}','model':data['model']}})
                await ws.send_json({'type':'response.completed','response':{'id':f'r{i}','reasoning':{'effort':'high'},'usage':{'output_tokens_details':{'reasoning_tokens':i}}}})
            return ws

        base = await self.setup_proxy(handler)
        async with self.client.ws_connect(base + '/v1/responses', headers={'Authorization':'Bearer SECRET'}) as ws:
            for i in range(2):
                await ws.send_json({'type':'response.create','model':f'm{i}','reasoning':{'effort':'high'},'index':i})
                self.assertEqual((await ws.receive_json())['response']['id'], f'r{i}')
                self.assertEqual((await ws.receive_json())['response']['usage']['output_tokens_details']['reasoning_tokens'], i)
        rows = list(self.journal.rows.values())
        self.assertEqual(len(rows), 2)
        self.assertTrue(all(r['model_comparison'] == 'same' and r['effort_comparison'] == 'same' for r in rows))

    async def test_http_errors_and_compression_are_forwarded(self):
        payload = gzip.compress(b'{"error":{"code":"invalid_model","message":"SECRET"}}')
        async def handler(request):
            return web.Response(status=429, body=payload, headers={'Content-Encoding':'gzip','Content-Type':'application/json','Retry-After':'3'})
        base = await self.setup_proxy(handler)
        async with self.client.post(base + '/v1/responses', json={'model':'x'}) as res:
            self.assertEqual(res.status, 429)
            self.assertEqual(res.headers['Retry-After'], '3')
            self.assertEqual(await res.read(), payload)
        self.assertEqual(list(self.journal.rows.values())[0]['response']['error_code'], 'invalid_model')

    async def test_no_redirect_follow_and_cross_origin_guard(self):
        async def handler(request):
            return web.Response(status=307, headers={'Location':'https://example.invalid/'})
        base = await self.setup_proxy(handler)
        async with self.client.post(base + '/v1/responses', json={'model':'x'}, allow_redirects=False) as res:
            self.assertEqual(res.status, 307)
        async with self.client.get(base + '/__wiretap__/api/records', headers={'Origin':'https://example.invalid'}) as res:
            self.assertEqual(res.status, 403)
        async with self.client.get(base + '/v2/responses') as res:
            self.assertEqual(res.status, 404)

    async def test_failed_websocket_handshake_allows_fallback(self):
        async def handler(request):
            return web.Response(status=426)
        base = await self.setup_proxy(handler)
        async with self.client.get(base + '/v1/responses', headers={'Upgrade':'websocket'}) as res:
            self.assertEqual(res.status, 426)


if __name__ == '__main__':
    unittest.main()
