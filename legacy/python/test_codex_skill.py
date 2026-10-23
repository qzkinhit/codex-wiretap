import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parent / 'skills/codex/scripts/codex_run.py'
SESSION = '00000000-0000-4000-8000-000000000001'
# Stand-in for the Codex CLI: records its call, edits cwd, writes a session record and a final message.
FAKE_CODEX = textwrap.dedent('''\
    #!/usr/bin/env python3
    import json, os, pathlib, sys, time
    args = sys.argv[1:]
    prompt = sys.stdin.read()
    with open(os.environ['FAKE_CODEX_LOG'], 'a', encoding='utf-8') as log:
        log.write(json.dumps({'args': args, 'cwd': os.getcwd(), 'prompt': prompt}) + '\\n')
    pathlib.Path('made_by_codex.txt').write_text('ok')
    day = pathlib.Path(os.environ['CODEX_HOME'], 'sessions', '2026', '01', '01')
    day.mkdir(parents=True, exist_ok=True)
    now = time.strftime('%Y-%m-%dT%H:%M:%S.999Z', time.gmtime())
    rows = [{'timestamp': now, 'type': 'turn_context',
             'payload': {'model': 'model-example', 'effort': 'high', 'sandbox_policy': {'type': 'workspace-write'}}},
            {'timestamp': now, 'type': 'event_msg',
             'payload': {'type': 'token_count', 'info': {'last_token_usage': {'input_tokens': int(os.environ['FAKE_CONTEXT'])}}}}]
    with open(day / 'rollout-2026-01-01T00-00-00-SESSION.jsonl', 'a', encoding='utf-8') as fh:
        fh.writelines(json.dumps(row) + '\\n' for row in rows)
    print(json.dumps({'type': 'thread.started', 'thread_id': 'SESSION'}))
    print(json.dumps({'type': 'item.completed', 'item': {'type': 'command_execution', 'command': 'python3 check.py', 'exit_code': 1}}))
    print(json.dumps({'type': 'item.completed', 'item': {'type': 'command_execution', 'command': 'python3 - <<EOF\\nprint(1)\\nEOF', 'exit_code': 0}}))
    pathlib.Path(args[args.index('-o') + 1]).write_text('完成情况 已完成', encoding='utf-8')
''').replace('SESSION', SESSION)


class CodexSkillTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        subprocess.run(['git', 'init', '-q', str(self.repo)], check=True)
        fake = self.root / 'codex'
        fake.write_text(FAKE_CODEX, encoding='utf-8')
        fake.chmod(0o755)
        self.log = self.root / 'codex-calls.jsonl'
        self.brief = self.root / 'brief.md'
        self.brief.write_text('画一张示例图。\n', encoding='utf-8')
        self.runs = self.root / 'runs'
        # Empty values also mask any developer-local skills/codex/.env.
        self.env = dict(os.environ, CODEX_BIN=str(fake), CODEX_HOME=str(self.root / 'codex-home'),
                        CODEX_RUNS_ROOT=str(self.runs), CODEX_MODEL='', CODEX_EFFORT='', CODEX_NETWORK='',
                        CODEX_SANDBOX='', CODEX_CONTEXT_LIMIT='', FAKE_CODEX_LOG=str(self.log),
                        FAKE_CONTEXT='1000', PYTHONIOENCODING='utf-8')

    def tearDown(self):
        self.tmp.cleanup()

    def run_skill(self, *args, **env):
        return subprocess.run([sys.executable, str(SCRIPT), *map(str, args)], env={**self.env, **env},
                              capture_output=True, text=True, encoding='utf-8')

    def calls(self):
        return [json.loads(line) for line in self.log.read_text(encoding='utf-8').splitlines()]

    def run_dir(self):
        (run,) = self.runs.iterdir()
        return run

    def test_new_task_reports_session_context_and_changes(self):
        result = self.run_skill('new', self.repo, self.brief)
        self.assertEqual(result.returncode, 0, result.stderr)
        (call,) = self.calls()
        self.assertEqual(Path(call['cwd']).resolve(), self.repo)
        self.assertEqual(call['args'][0], 'exec')
        self.assertEqual(call['args'][call['args'].index('-s') + 1], 'workspace-write')
        self.assertNotIn('-m', call['args'])
        self.assertTrue(call['prompt'].startswith('# 执行约定'))
        self.assertTrue(call['prompt'].endswith('画一张示例图。\n'))
        for text in (SESSION, '模型 model-example，思考强度 high', '上下文 1,000 tokens', '完成情况 已完成',
                     '! python3 check.py', '  python3 - <<EOF\\nprint(1)\\nEOF\n',
                     str(self.repo / 'made_by_codex.txt'), '?? made_by_codex.txt'):
            self.assertIn(text, result.stdout)

    def test_model_and_effort_pins_reach_codex(self):
        self.run_skill('new', self.repo, self.brief, CODEX_MODEL='model-example', CODEX_EFFORT='high')
        args = self.calls()[0]['args']
        self.assertEqual(args[args.index('-m') + 1], 'model-example')
        self.assertIn('model_reasoning_effort="high"', args)

    def test_resume_reuses_session_without_preamble(self):
        self.run_skill('new', self.repo, self.brief)
        follow = self.root / 'follow.md'
        follow.write_text('改成红色。\n', encoding='utf-8')
        result = self.run_skill('resume', self.run_dir(), follow)
        self.assertEqual(result.returncode, 0, result.stderr)
        second = self.calls()[1]
        self.assertEqual(second['args'][:2], ['exec', 'resume'])
        self.assertEqual(second['args'][-2:], [SESSION, '-'])
        self.assertIn('sandbox_mode="workspace-write"', second['args'])
        self.assertEqual(second['prompt'], '改成红色。\n')
        self.assertIn('第 2 轮', result.stdout)

    def test_resume_is_refused_near_context_limit(self):
        first = self.run_skill('new', self.repo, self.brief, FAKE_CONTEXT='200000')
        self.assertIn('不再续接', first.stdout)
        result = self.run_skill('resume', self.run_dir(), self.brief)
        self.assertEqual(result.returncode, 3)
        self.assertEqual(len(self.calls()), 1)

    def test_review_runs_in_isolated_workspace(self):
        result = self.run_skill('review', self.brief, CODEX_SANDBOX='danger-full-access')
        self.assertEqual(result.returncode, 0, result.stderr)
        (call,) = self.calls()
        self.assertEqual(Path(call['cwd']).resolve(), self.run_dir() / 'workspace')
        self.assertEqual(call['args'][call['args'].index('-s') + 1], 'workspace-write')
        self.assertTrue(call['prompt'].startswith('# 审查约定'))
        self.assertFalse((self.repo / 'made_by_codex.txt').exists())


if __name__ == '__main__':
    unittest.main()
