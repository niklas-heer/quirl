"""Replay frozen Jev cases through tool-disabled, independent Codex CLI turns.

Failure model: the CLI can fail, hang, emit malformed/flooding JSONL, or attempt
a tool call. Bound input/output and wall time, reject tool events, and kill/reap
the process group on every exit. Never execute or repair a model-selected command.
Only supplied synthetic tasks and catalog metadata belong in persisted results.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import selectors
import signal
import statistics
import subprocess
import tempfile
import time

EVIDENCE = Path(__file__).with_name('evidence.json')
MODEL = 'gpt-5.6-luna'
DISABLED = ['shell_tool', 'unified_exec', 'apps', 'browser_use', 'computer_use',
            'multi_agent', 'plugins', 'hooks', 'shell_snapshot', 'image_generation',
            'view_image', 'sleep_tool', 'goals', 'skill_search']
PREFIX = ('Select exactly one option for this classification task. Do not use tools, '
          'inspect files, or execute any command. Return only the schema-constrained choice.\n')


def command(work, schema, effort):
    args = ['codex', 'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral',
            '--skip-git-repo-check', '--sandbox', 'read-only', '--model', MODEL,
            '-c', f'model_reasoning_effort="{effort}"', '-c', 'web_search="disabled"',
            '-c', 'project_doc_max_bytes=0', '-c', 'suppress_unstable_features_warning=true',
            '--enable', 'skip_host_skill_discovery']
    for feature in DISABLED:
        args.extend(['--disable', feature])
    return args + ['--output-schema', str(schema), '--color', 'never', '--json',
                   '--cd', str(work), '-']


def frozen_cases(evidence):
    for group in evidence['sets']:
        for case in group['cases']:
            question = case['request']['questions']['selection']
            choices = evidence['criteria_maps'][question['criteria_ref']]
            options = {name: choices[name] for name in question['criteria_order']}
            payload = {'state': case['request']['state'],
                       'instructions': question['instructions'], 'options': options}
            expected = case['expected']
            for correction in evidence['corrections']:
                if correction['set'] == group['name'] and correction['case'] == case['id']:
                    expected = correction['accepted_after_review']
            yield dict(set=group['name'], id=case['id'], category=case['category'],
                       expected=expected, original_expected=case['expected'], payload=payload)


def invoke(args, prompt):
    """Keep pipe ownership, deadline, admission, and cleanup in one state machine."""
    if len(prompt) > 200_000:
        raise ValueError('prompt byte bound')
    start = time.perf_counter()
    deadline = time.monotonic() + 30
    environment = dict(os.environ)
    for name in ('BASH_ENV', 'ENV', 'ZDOTDIR'):
        environment.pop(name, None)
    process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True,
                               env=environment)
    streams = {'stdout': bytearray(), 'stderr': bytearray()}
    offsets = 0
    events = []
    pending = bytearray()
    try:
        with selectors.DefaultSelector() as selector:
            for stream, label, mode in ((process.stdin, 'stdin', selectors.EVENT_WRITE),
                                       (process.stdout, 'stdout', selectors.EVENT_READ),
                                       (process.stderr, 'stderr', selectors.EVENT_READ)):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, mode, label)
            while selector.get_map():
                if time.monotonic() >= deadline:
                    raise TimeoutError('30 second request deadline')
                for selected, _ in selector.select(0.1):
                    stream, label = selected.fileobj, selected.data
                    if label == 'stdin':
                        offsets += os.write(stream.fileno(), prompt[offsets:offsets + 8192])
                        if offsets == len(prompt):
                            selector.unregister(stream)
                            stream.close()
                        continue
                    data = os.read(stream.fileno(), 8192)
                    if not data:
                        selector.unregister(stream)
                        stream.close()
                        continue
                    streams[label].extend(data)
                    if len(streams[label]) > 1_000_000:
                        raise ValueError('one MB stream bound')
                    if label == 'stdout':
                        pending.extend(data)
                        while b'\n' in pending:
                            line, _, rest = pending.partition(b'\n')
                            pending = bytearray(rest)
                            if line:
                                events.append((round((time.perf_counter() - start) * 1000, 2),
                                               json.loads(line)))
                                if len(events) > 100:
                                    raise ValueError('event count bound')
                                item = events[-1][1].get('item', {})
                                if item.get('type') not in (None, 'agent_message', 'reasoning', 'error'):
                                    raise ValueError('unexpected tool or effect event')
            if pending.strip():
                raise ValueError('unterminated JSONL event')
            process.wait(timeout=max(0.01, deadline - time.monotonic()))
        if process.returncode:
            raise ValueError(f'CLI exit {process.returncode}')
        return dict(process_ms=round((time.perf_counter() - start) * 1000, 2),
                    stderr_bytes=len(streams['stderr']), events=events)
    finally:
        # No generated subprocess is permitted; still contain startup/exit failures.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=5)
        for stream in (process.stdin, process.stdout, process.stderr):
            if not stream.closed:
                stream.close()


def run(output, effort, limit):
    evidence_bytes = EVIDENCE.read_bytes()
    if len(evidence_bytes) > 2_000_000:
        raise ValueError('evidence byte bound')
    evidence = json.loads(evidence_bytes)
    cases = list(frozen_cases(evidence))
    if not 1 <= len(cases) <= 150:
        raise ValueError('case count bound')
    cases = cases[:limit]
    output.mkdir(parents=True, exist_ok=False)
    version = subprocess.run(['codex', '--version'], capture_output=True, text=True,
                             check=True, timeout=5).stdout.strip()
    metadata = dict(model=MODEL, reasoning_effort=effort, cli_version=version,
                    evidence_sha256=hashlib.sha256(evidence_bytes).hexdigest(),
                    case_count=len(cases), disabled_features=DISABLED,
                    authentication='existing ChatGPT CLI login', prompt_prefix=PREFIX,
                    max_request_seconds=30, stream_bytes_max=1_000_000,
                    comparison='CLI process per case; Jev used a persistent HTTPS connection',
                    pricing=dict(input_per_million_usd=0.20, cached_input_per_million_usd=0.02,
                                 output_per_million_usd=1.20,
                                 source='https://developers.openai.com/api/docs/models/gpt-5.6-luna',
                                 interpretation='API-equivalent estimate, not the ChatGPT bill'))
    (output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    all_start = time.monotonic()
    with tempfile.TemporaryDirectory(prefix='quirl-luna-work-') as directory:
        work = Path(directory)
        schema = work / 'schema.json'
        with (output / 'results.jsonl').open('x') as log:
            consecutive_errors = 0
            for index, case in enumerate(cases):
                if time.monotonic() - all_start > 1200:
                    raise TimeoutError('20 minute experiment budget')
                options = case['payload']['options']
                schema.write_text(json.dumps(dict(type='object', properties={
                    'choice': dict(type='string', enum=list(options))},
                    required=['choice'], additionalProperties=False)))
                prompt = (PREFIX + json.dumps(case['payload'], ensure_ascii=False)).encode()
                row = {k: v for k, v in case.items() if k != 'payload'}
                row['prompt_sha256'] = hashlib.sha256(prompt).hexdigest()
                begin = time.perf_counter()
                try:
                    result = invoke(command(work, schema, effort), prompt)
                    row.update(process_ms=result['process_ms'], stderr_bytes=result['stderr_bytes'])
                    final = []
                    usage = None
                    turn_start = turn_end = None
                    for elapsed, event in result['events']:
                        kind = event.get('type')
                        if kind == 'turn.started':
                            turn_start = elapsed
                        if kind == 'turn.completed':
                            turn_end = elapsed
                            usage = event['usage']
                        if kind == 'item.completed' and event.get('item', {}).get('type') == 'agent_message':
                            final.append(event['item']['text'])
                        if kind in ('error', 'turn.failed') or event.get('item', {}).get('type') == 'error':
                            row.setdefault('warnings', []).append(event)
                    if len(final) != 1 or usage is None or turn_start is None or turn_end is None:
                        raise ValueError('missing or multiple completion records')
                    row.update(final_text=final[0], usage=usage, turn_ms=round(turn_end-turn_start, 2))
                    answer = json.loads(final[0])
                    if set(answer) != {'choice'} or answer['choice'] not in options:
                        raise ValueError('answer schema mismatch')
                    row.update(choice=answer['choice'], correct=answer['choice'] in case['expected'])
                    consecutive_errors = 0
                except Exception as error:
                    row.update(error=type(error).__name__ + ': ' + str(error),
                               process_ms=round((time.perf_counter()-begin)*1000, 2))
                    consecutive_errors += 1
                log.write(json.dumps(row, ensure_ascii=False) + '\n')
                log.flush()
                print(f"{index+1:03}/{len(cases)} {case['set']} {case['id']}: "
                      f"{row.get('choice', row.get('error'))} "
                      f"{'OK' if row.get('correct') else 'MISS'} {row['process_ms']:.0f}ms", flush=True)
                if consecutive_errors >= 3:
                    print('Stopping after three consecutive errors.', flush=True)
                    break
    summarize(output)


def summarize(output):
    rows = [json.loads(line) for line in (output / 'results.jsonl').read_text().splitlines()]
    values = sorted(row['process_ms'] for row in rows)
    usage = {key: sum(row.get('usage', {}).get(key, 0) for row in rows)
             for key in ('input_tokens', 'cached_input_tokens', 'cache_write_input_tokens',
                         'output_tokens', 'reasoning_output_tokens')}
    costs = ((usage['input_tokens'] - usage['cached_input_tokens']) * 0.20
             + usage['cached_input_tokens'] * 0.02 + usage['output_tokens'] * 1.20) / 1e6
    report = dict(total=len(rows), correct=sum(bool(row.get('correct')) for row in rows),
                  errors=sum('error' in row for row in rows), p50_ms=statistics.median(values),
                  p95_ms=values[math.ceil(len(values)*0.95)-1], usage=usage,
                  estimated_api_equivalent_usd=costs,
                  misses=[row for row in rows if not row.get('correct')])
    (output / 'summary.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--effort', choices=['low', 'medium', 'high'], default='low')
    parser.add_argument('--limit', type=int, choices=range(1, 145), default=144)
    args = parser.parse_args()
    run(args.output, args.effort, args.limit)
