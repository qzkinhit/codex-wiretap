#!/usr/bin/env python3
"""从 Claude Code 把任务派给 Codex CLI，并打印供审核的摘要。需要 Python 3.9+。

用法
  codex_run.py new <工作目录> <任务说明.md> [额外可写目录...]   画图与执行任务
  codex_run.py review <审查说明.md>                             定理与理论审查，在独立 workspace 中运行
  codex_run.py resume <运行目录> <返工说明.md>                  续接同一会话返工
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

SKILL = Path(__file__).resolve().parent.parent
APP_BINARIES = ("/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
                "/Applications/Codex.app/Contents/Resources/codex")
SKIP_DIRS = {".git", "node_modules", "__pycache__", ".ipynb_checkpoints", ".mypy_cache", ".pytest_cache"}
RESUME_RATIO = 0.75
LISTED = 200


def setting(name, default=""):
    return os.environ.get(name) or default


def load_env_file():
    """Per-user defaults live in <skill>/.env. Variables already set in the environment win."""
    path = SKILL / ".env"
    if not path.is_file():
        return
    for line in path.read_text(encoding="utf-8").splitlines():
        key, sep, value = line.strip().partition("=")
        if sep and key and not key.startswith("#"):
            os.environ.setdefault(key.strip(), value.strip().strip("\"'"))


def codex_binary():
    bundled = next((p for p in APP_BINARIES if os.access(p, os.X_OK)), None)
    return setting("CODEX_BIN") or bundled or shutil.which("codex") or "codex"


def fail(message, code=2):
    print(message, file=sys.stderr)
    sys.exit(code)


def save_state(run, state):
    (run / "state.json").write_text(json.dumps(state, ensure_ascii=False, indent=1), encoding="utf-8")


def start_run(mode, args, root):
    if mode == "new" and len(args) >= 2:
        workdir, brief, extra = Path(args[0]).expanduser().resolve(), Path(args[1]), args[2:]
        if not workdir.is_dir():
            fail(f"工作目录不存在 {workdir}")
        label = workdir.name
    elif mode == "review" and len(args) == 1:
        workdir, brief, extra = None, Path(args[0]), []
        label = brief.stem
    else:
        fail(__doc__)
    extra = [Path(d).expanduser().resolve() for d in extra]
    for d in extra:
        if not d.is_dir():
            fail(f"额外可写目录不存在 {d}")
    if not brief.is_file():
        fail(f"任务说明文件不存在 {brief}")
    run = root / f"{time.strftime('%Y%m%d-%H%M%S')}-{mode}-{label}"
    if run.exists():
        run = run.with_name(f"{run.name}-{os.getpid()}")
    run.mkdir(parents=True)
    if workdir is None:
        workdir = run / "workspace"
        workdir.mkdir()
    sandbox = "workspace-write" if mode == "review" else setting("CODEX_SANDBOX", "workspace-write")
    state = {"mode": mode, "workdir": str(workdir), "add_dirs": [str(d) for d in extra],
             "sandbox": sandbox, "session_id": None, "rounds": 0, "context": None}
    save_state(run, state)
    return run, state, brief


def load_run(args, limit):
    if len(args) != 2:
        fail(__doc__)
    run, brief = Path(args[0]).expanduser().resolve(), Path(args[1])
    if not (run / "state.json").is_file():
        fail(f"{run} 不是 codex_run.py 的运行目录")
    state = json.loads((run / "state.json").read_text(encoding="utf-8"))
    if not state.get("session_id"):
        fail("运行目录中没有会话 id，无法续接")
    context = state.get("context")
    if context is not None and context >= limit * RESUME_RATIO:
        fail(f"上一轮上下文 {context:,} tokens，已达到上限 {limit:,} 的 {RESUME_RATIO:.0%}，不再续接。"
             "请新开任务，把需要延续的结论写进任务说明。", 3)
    if not brief.is_file():
        fail(f"返工说明文件不存在 {brief}")
    return run, state, brief


def build_command(state, last_message, first):
    cmd = [codex_binary(), "exec"] + ([] if first else ["resume"])
    cmd += ["--skip-git-repo-check", "--json", "-o", str(last_message)]
    if setting("CODEX_MODEL"):
        cmd += ["-m", setting("CODEX_MODEL")]
    if setting("CODEX_EFFORT"):
        cmd += ["-c", f'model_reasoning_effort="{setting("CODEX_EFFORT")}"']
    if setting("CODEX_NETWORK") == "1":
        cmd += ["-c", "sandbox_workspace_write.network_access=true"]
    if first:
        cmd += ["-C", state["workdir"], "-s", state["sandbox"]]
        for d in state["add_dirs"]:
            cmd += ["--add-dir", d]
    else:
        # exec resume has no -s, -C or --add-dir flags, so the same settings go through -c.
        cmd += ["-c", f'sandbox_mode="{state["sandbox"]}"']
        if state["add_dirs"]:
            cmd += ["-c", "sandbox_workspace_write.writable_roots=" + json.dumps(state["add_dirs"])]
        cmd.append(state["session_id"])
    return cmd + ["-"]


def git_status(path):
    out = subprocess.run(["git", "-C", str(path), "status", "--porcelain=v1", "-uall"],
                         capture_output=True, text=True)
    return out.stdout.splitlines() if out.returncode == 0 else None


def modified_since(roots, marker_ns, skip):
    hits = []
    for root in roots:
        for top, dirs, files in os.walk(root):
            dirs[:] = [d for d in dirs if d not in SKIP_DIRS and not d.startswith(".venv")
                       and os.path.join(top, d) != skip]
            for name in files:
                path = os.path.join(top, name)
                try:
                    if os.stat(path).st_mtime_ns > marker_ns:
                        hits.append(path)
                except OSError:
                    pass
    return sorted(hits)


def json_rows(path):
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if isinstance(row, dict):
                yield row


def read_events(path):
    session_id, commands, errors = None, [], []
    for event in json_rows(path):
        kind, item = event.get("type"), event.get("item") or {}
        if kind == "thread.started":
            session_id = session_id or event.get("thread_id")
        elif kind == "item.completed" and item.get("type") == "command_execution":
            commands.append((item.get("exit_code"), str(item.get("command", ""))))
        elif kind in ("error", "turn.failed"):
            errors.append(str(event.get("message") or json.dumps(event.get("error"), ensure_ascii=False)))
    return session_id, commands, errors


def session_facts(run, session_id, since):
    """Model, effort, sandbox and last request size that Codex recorded for this round."""
    if not session_id:
        return {}
    cache = run / "rollout_path"
    path = Path(cache.read_text(encoding="utf-8").strip()) if cache.is_file() else None
    if path is None or not path.is_file():
        sessions = Path(setting("CODEX_HOME", "~/.codex")).expanduser() / "sessions"
        hits = sorted(sessions.glob(f"*/*/*/rollout-*{session_id}.jsonl"))
        if not hits:
            return {}
        path = hits[-1]
        cache.write_text(str(path), encoding="utf-8")
    facts = {}
    for row in json_rows(path):
        if str(row.get("timestamp", "")) < since:
            continue
        payload = row.get("payload") or {}
        if row.get("type") == "turn_context":
            facts["model"] = payload.get("model")
            facts["effort"] = payload.get("effort") or payload.get("reasoning_effort")
            facts["sandbox"] = (payload.get("sandbox_policy") or {}).get("type")
        elif payload.get("type") == "token_count":
            usage = (payload.get("info") or {}).get("last_token_usage") or {}
            if "input_tokens" in usage:
                facts["context"] = usage["input_tokens"]
    return facts


def report(run, state, round_dir, code, elapsed, facts, events, changed, new_status, limit):
    _, commands, errors = events
    print(f"== Codex 第 {state['rounds']} 轮结束，exit={code}，用时 {elapsed}s")
    print(f"运行目录 {run}")
    print(f"会话 {state['session_id'] or '未知'}")
    if facts.get("model") or facts.get("effort"):
        print(f"模型 {facts.get('model') or '未知'}，思考强度 {facts.get('effort') or '未知'}，"
              f"沙箱 {facts.get('sandbox') or '未知'}")
    context = facts.get("context")
    if context is None:
        print("会话记录中没有找到上下文用量")
    else:
        print(f"上下文 {context:,} tokens，为上限 {limit:,} 的 {context / limit:.0%}")
        if context >= limit * RESUME_RATIO:
            print(f"上下文已达到上限的 {RESUME_RATIO:.0%}，此会话不再续接。返工请新开任务。")
    if errors:
        print(f"事件流中有 {len(errors)} 条错误，最后一条为 {errors[-1][:300]}")
    print("\n== Codex 最终报告")
    last = round_dir / "last_message.md"
    if last.is_file() and last.stat().st_size:
        print(last.read_text(encoding="utf-8", errors="replace").rstrip())
    else:
        print("没有最终报告。stderr 末尾如下")
        lines = (round_dir / "stderr.log").read_text(encoding="utf-8", errors="replace").splitlines()
        print("\n".join(lines[-20:]))
    print("\n== 执行过的命令，退出码非 0 的以 ! 开头")
    for exit_code, command in commands[-60:]:
        print(("! " if exit_code not in (0, None) else "  ") + command.replace("\n", "\\n")[:220])
    print("\n== 本轮修改过的文件，按修改时间判断，可能包含其他进程的写入")
    print("\n".join(changed[:LISTED]))
    if len(changed) > LISTED:
        print(f"另有 {len(changed) - LISTED} 个文件未列出")
    if new_status is not None:
        print("\n== git status 中开工前没有的条目")
        print("\n".join(new_status[:LISTED]))


def main(argv):
    load_env_file()
    if not argv or argv[0] not in ("new", "review", "resume"):
        fail(__doc__)
    mode, args = argv[0], argv[1:]
    limit = int(setting("CODEX_CONTEXT_LIMIT", "256000"))
    if mode == "resume":
        run, state, brief = load_run(args, limit)
    else:
        run, state, brief = start_run(mode, args, Path(setting("CODEX_RUNS_ROOT", "~/.claude/codex-runs")).expanduser())

    first = state["rounds"] == 0
    round_dir = run / f"round{state['rounds'] + 1}"
    round_dir.mkdir()
    prompt = brief.read_text(encoding="utf-8")
    if first:
        preamble = "preamble_review.md" if state["mode"] == "review" else "preamble_exec.md"
        prompt = (SKILL / preamble).read_text(encoding="utf-8") + prompt
    (round_dir / "prompt.md").write_text(prompt, encoding="utf-8")
    cmd = build_command(state, round_dir / "last_message.md", first)

    workdir = Path(state["workdir"])
    before = git_status(workdir)
    marker = round_dir / ".start"
    marker.touch()
    started = time.time()
    since = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(started))
    try:
        with (round_dir / "prompt.md").open("rb") as stdin, (round_dir / "events.jsonl").open("wb") as out, \
                (round_dir / "stderr.log").open("wb") as err:
            code = subprocess.run(cmd, cwd=workdir, stdin=stdin, stdout=out, stderr=err).returncode
    except OSError as exc:
        fail(f"无法启动 codex，{exc}", 127)
    elapsed = int(time.time() - started)

    events = read_events(round_dir / "events.jsonl")
    state["session_id"] = state["session_id"] or events[0]
    state["rounds"] += 1
    facts = session_facts(run, state["session_id"], since)
    state["context"] = facts.get("context")
    save_state(run, state)
    roots = [workdir] + [Path(d) for d in state["add_dirs"]]
    changed = modified_since(roots, marker.stat().st_mtime_ns, str(run))
    after = git_status(workdir) if before is not None else None
    new_status = sorted(set(after) - set(before)) if after is not None else None
    report(run, state, round_dir, code, elapsed, facts, events, changed, new_status, limit)
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
