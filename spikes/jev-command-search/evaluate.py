"""Bounded, read-only Jev command-selection experiment; never executes choices."""
import collections
import hashlib
import http.client
import json
import math
import pathlib
import random
import sqlite3
import statistics
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/jev-command-search'
MODEL = 'jev-1.13.0'

# Expected answers are frozen before any inference. Multiple equivalent tools
# are accepted; this measures discovery, not argument construction or execution.
CASES = [
 ('basic','Encrypt a file for a recipient using their public key.',['age']),
 ('basic','Calculate an expression with arbitrary decimal precision.',['bc']),
 ('basic','Decompress a file ending in .br.',['brotli']),
 ('basic','Print two text files consecutively to standard output.',['cat']),
 ('basic','Change the current shell working directory.',['cd']),
 ('basic','Open this project in Visual Studio Code.',['code']),
 ('basic','Make a second copy of a folder and its contents.',['cp']),
 ('basic','Display a Git diff with enhanced syntax highlighting.',['delta']),
 ('basic','Compare two text files and show their differing lines.',['diff']),
 ('basic','Look up the MX records of a domain.',['dig']),
 ('basic','Start the services described in my compose file.',['docker compose']),
 ('basic','Convert Windows CRLF line endings to Unix LF.',['dos2unix']),
 ('basic','Find files by their filename rather than their contents.',['fd']),
 ('basic','Interactively choose an item by fuzzy matching a list.',['fzf']),
 ('basic','Add a new remote named upstream to my Git repository.',['git remote add']),
 ('basic','Change the URL of an existing Git remote.',['git remote set-url']),
 ('basic','Extract a nested field from a JSON document.',['jq']),
 ('basic','Run a recipe defined in a justfile.',['just']),
 ('basic','Build a target specified in a Makefile.',['make']),
 ('basic','Read the system manual page for a command.',['man']),
 ('basic','Create a directory with missing parent directories.',['mkdir']),
 ('basic','Rename a file without retaining the old name.',['mv']),
 ('basic','Execute a JavaScript file with Node.js.',['node']),
 ('basic','Print the absolute path of the current working directory.',['pwd']),
 ('basic','Execute a Python script.',['python']),
 ('basic','Run an SQL query against a SQLite database file.',['sqlite3']),
 ('basic','Log in to a remote server over an encrypted shell connection.',['ssh']),
 ('basic','Copy one file to a remote machine over SSH.',['scp','rsync']),
 ('basic','Synchronize a directory to a server, transferring only changes.',['rsync']),
 ('basic','Compile a Rust source file into a program.',['rustc']),
 ('basic','Print the directory hierarchy as a tree.',['tree','eza']),
 ('basic','Download a file from an HTTPS URL.',['wget']),
 ('contrast','Show the beginning of a long log, not its latest entries.',['head']),
 ('contrast','Keep watching new lines appended to a running log.',['tail']),
 ('contrast','Remove a directory only if it is empty.',['rmdir']),
 ('contrast','Remove a directory together with every file inside it.',['rm']),
 ('contrast','Find occurrences of a word inside files, not matching filenames.',['rg','grep']),
 ('contrast','List members of a ZIP archive without extracting it.',['unzip']),
 ('contrast','List members of a tar archive without extracting it.',['tar']),
 ('contrast','Generate Task command completions specifically for zsh.',['task completion zsh']),
 ('multilingual','Zeige mir die letzten zwanzig Zeilen einer Datei.',['tail']),
 ('multilingual','Benenne eine Datei um, ohne eine Kopie zu behalten.',['mv']),
 ('multilingual','Suche Dateien anhand ihres Namens, nicht ihres Inhalts.',['fd']),
 ('multilingual','Ändere die Adresse eines vorhandenen Git-Remotes.',['git remote set-url']),
 ('multilingual','Extrae un campo de un documento JSON.',['jq']),
 ('multilingual','Afficher les premières lignes de ce fichier.',['head']),
 ('multilingual','查询这个域名的 DNS 记录。',['dig']),
 ('multilingual','空のディレクトリだけを削除したい。',['rmdir']),
 ('unsupported','Resize a JPEG image to 800 pixels wide.',['NONE']),
 ('unsupported','Play this MP3 through the speakers.',['NONE']),
 ('unsupported','List which local processes consume the most CPU.',['NONE']),
 ('unsupported','Show filesystem free and used disk capacity.',['NONE']),
 ('ambiguous','Clean up my project.',['CLARIFY']),
 ('ambiguous','Fix the remote.',['CLARIFY']),
 ('ambiguous','Do something with this file.',['CLARIFY']),
 ('ambiguous','Make it faster.',['CLARIFY']),
 ('composition','Download a ZIP from a URL and then extract its contents.',['COMPOSE']),
 ('composition','Find matching error lines in a log and send those lines over SSH to another host.',['COMPOSE']),
 ('composition','Create a directory and then change the current shell into it.',['COMPOSE']),
 ('composition','Encrypt a local document and then upload the encrypted file over SSH.',['COMPOSE']),
]
FLAGS = [
 ('rg','Interpret the search text literally, including dots and brackets.',['--fixed-strings']),
 ('rg','Show only paths of files that contain a match.',['--files-with-matches']),
 ('rg','Show only paths of files that contain no matches.',['--files-without-match']),
 ('rg','Print the number of matching lines in each file, not the number of individual matches.',['--count']),
 ('rg','Print the number of individual matches, including multiple on one line.',['--count-matches']),
 ('rg','Show three lines both before and after each matching line.',['--context']),
 ('rg','Ignore capitalization when matching the pattern.',['--ignore-case']),
 ('rg','Include hidden files and directories in the search.',['--hidden']),
 ('rm','Prompt before every single file removal.',['-i']),
 ('rm','Prompt once before risky removals, rather than for every file.',['-I']),
 ('rm','Recursively remove a directory and its contents.',['-r','-R']),
 ('cp','Refuse to overwrite existing destination files.',['-n']),
 ('cp','Ask before overwriting an existing destination file.',['-i']),
 ('head','Limit output by bytes instead of by lines.',['--bytes']),
 ('tail','Continue displaying data appended after opening the file.',['-f']),
 ('git remote set-url','Change only the URL used for pushing.',['--push']),
]

def fixture():
    database = ROOT / 'catalog/generated/catalog.sqlite3'
    with sqlite3.connect(f'file:{database}?mode=ro', uri=True) as db:
        catalog = {name: summary + ' ' + description for name, summary, description in
                   db.execute('SELECT full_path, summary, description FROM commands ORDER BY full_path')}
        command_options = dict(catalog)
        command_options.update({
            'NONE':'No listed command directly supports this task. Do not invent scripts, plugins, subprocess tricks, or new commands.',
            'CLARIFY':'The request is too ambiguous to choose a specific useful command; ask the user what outcome they want.',
            'COMPOSE':'The requested final outcome requires composing multiple listed commands; no single listed command directly satisfies it.',
        })
        tests = []
        for i,(category,query,expected) in enumerate(CASES):
            tests.append(dict(id=f'command-{i:02}', category=category, query=query,
                              expected=expected, options=command_options))
        for i,(command,query,expected) in enumerate(FLAGS):
            options = {}
            for name,short,summary in db.execute('''SELECT f.name,f.short_name,f.summary FROM flags f
              JOIN commands c USING(command_id) WHERE c.full_path=? AND
              (NOT EXISTS(SELECT 1 FROM flag_platforms p WHERE p.flag_id=f.flag_id)
               OR EXISTS(SELECT 1 FROM flag_platforms p WHERE p.flag_id=f.flag_id AND p.platform IN ('any','macos')))
              ORDER BY f.name''',(command,)):
                options[name] = (short + ': ' if short else '') + summary
            missing=set(expected)-options.keys()
            if missing:
                raise ValueError(f'Expected option absent: {command} {sorted(missing)}')
            options['NONE']='None of these flags matches the requested behavior.'
            tests.append(dict(id=f'flag-{i:02}', category='flag', query=query,
                              command=command, expected=expected, options=options))
    result=dict(model=MODEL, platform='macos', catalog_sha256=hashlib.sha256(database.read_bytes()).hexdigest(),
                command_count=len(catalog), tests=tests)
    OUT.mkdir(parents=True,exist_ok=True)
    path=OUT/'fixture.json'
    encoded=json.dumps(result,ensure_ascii=False,indent=2)
    with path.open('x') as f:
        f.write(encoded+'\n')
    print('Frozen fixture:',len(tests),'cases;',len(catalog),'commands;',hashlib.sha256(encoded.encode()).hexdigest())
    return result

def request_for(case, reverse=False):
    items=list(case['options'].items())
    random.Random(case['id']).shuffle(items)
    if reverse:
        items.reverse()
    if case['category']=='flag':
        instruction='Select the single flag for `command` that best matches `request` on macOS. Select NONE if none fits. Return a flag, not its argument.'
    else:
        instruction=('Choose the most specific catalog command directly serving `request`, allowing its normal flags and arguments. '
          'Choose among the listed commands, NONE, CLARIFY, or COMPOSE. Do not substitute an interpreter running invented code, '
          'an arbitrary subprocess, or a partial step for the requested final outcome. This is discovery only; do not execute anything.')
    return dict(model=MODEL,state={k:case[k] for k in ('command',) if k in case} | {'request':case['query']},
                questions={'selection':dict(type='choice',instructions=instruction,criteria=dict(items))})

def evaluate(key):
    tests=json.loads((OUT/'fixture.json').read_text())['tests']
    assert 0<len(tests)<=100
    conn=http.client.HTTPSConnection('api.typesafe.ai',timeout=10)
    started=time.monotonic()
    with (OUT/'results.jsonl').open('x') as log:
        for index,case in enumerate(tests):
            if time.monotonic()-started>600:
                raise TimeoutError('Experiment wall budget exceeded')
            payload=json.dumps(request_for(case),ensure_ascii=False).encode()
            assert len(payload)<=200_000
            begin=time.perf_counter()
            row=dict(id=case['id'],category=case['category'],query=case['query'],expected=case['expected'])
            try:
                conn.request('POST','/v1/systemone',body=payload,headers={'Authorization':'Bearer '+key,'Content-Type':'application/json'})
                response=conn.getresponse()
                body=response.read(1_000_001)
                row.update(latency_ms=round((time.perf_counter()-begin)*1000,2),status=response.status)
                if len(body)>1_000_000:
                    raise ValueError('Response byte limit exceeded')
                if response.status!=200:
                    row['error']='HTTP '+str(response.status)
                    conn.close()
                else:
                    data=json.loads(body)
                    answer=data['answers']['selection']
                    row.update(answer=answer,usage=data.get('usage'),model=data.get('model'))
                    probabilities=answer['probabilities']
                    assert answer['choice'] in case['options']
                    assert set(probabilities)==set(case['options'])
                    assert all(math.isfinite(p) and 0<=p<=1 for p in probabilities.values())
                    assert abs(sum(probabilities.values())-1)<0.01
                    assert math.isfinite(answer['confidence']) and 0<=answer['confidence']<=1
                    ranked=sorted(probabilities,key=lambda x:probabilities[x],reverse=True)
                    row.update(choice=answer['choice'],correct=answer['choice'] in case['expected'],
                        confidence=answer['confidence'],top3=ranked[:3],
                        expected_rank=min(ranked.index(x)+1 for x in case['expected']),
                        answer=answer,usage=data.get('usage'),model=data.get('model'))
            except Exception as error:
                # Never serialize exceptions, requests, headers, or the key.
                row.update(error=type(error).__name__,latency_ms=round((time.perf_counter()-begin)*1000,2))
                conn.close()
            log.write(json.dumps(row,ensure_ascii=False)+'\n')
            log.flush()
            print(f"{index+1:02}/{len(tests)} {case['id']} {row.get('choice',row.get('error'))} {'OK' if row.get('correct') else 'MISS'} {row['latency_ms']:.0f}ms",flush=True)
            if row.get('status') in (401,403,429,529):
                print('Stopping on authentication, rate limit, or overload; no retries.')
                break
    conn.close()
    summarize()

def summarize():
    rows=[json.loads(line) for line in (OUT/'results.jsonl').read_text().splitlines()]
    groups=collections.defaultdict(list)
    for row in rows:
        groups[row['category']].append(row)
    summary={category:dict(total=len(items),correct=sum(bool(x.get('correct')) for x in items)) for category,items in groups.items()}
    values=sorted(x['latency_ms'] for x in rows)
    tokens=sum((r.get('usage') or {}).get('input_tokens',0) for r in rows)
    summary.update(total=len(rows),correct=sum(bool(x.get('correct')) for x in rows),
        errors=sum('error' in r for r in rows),p50_ms=statistics.median(values),p95_ms=values[math.ceil(len(values)*.95)-1],
        min_ms=values[0],max_ms=values[-1],input_tokens=tokens,estimated_usd=tokens*.042/1_000_000,
        misses=[{k:v for k,v in r.items() if k not in ('answer','usage')} for r in rows if not r.get('correct')])
    (OUT/'summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(summary,ensure_ascii=False,indent=2))

if __name__=='__main__':
    fixture()
