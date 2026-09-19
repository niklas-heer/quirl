"""Fresh challenge set frozen after the initial run; same unchanged prompt."""
import hashlib
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import evaluate as bench

BASE_OUTPUT = bench.OUT

FLAG_CASES = [
 ('cp','When traversing directories, copy symbolic links themselves; never follow them.',['-P']),
 ('cp','Follow symbolic links supplied as command arguments, but not links discovered inside directories.',['-H']),
 ('cp','Follow every symbolic link, including links discovered during recursive traversal.',['-L']),
 ('cp','Create hard links to the source files instead of duplicating their data.',['-l']),
 ('cp','Make symbolic links pointing at the sources instead of making copies.',['-s']),
 ('cp','Avoid crossing into another mounted filesystem while copying.',['-x']),
 ('cp','Copy the regular file data but omit extended attributes.',['-X']),
 ('rg','Let a pattern match text spanning multiple lines.',['--multiline']),
 ('rg','In multiline mode, make the dot metacharacter also match newlines.',['--multiline-dotall']),
 ('rg','Include files excluded by ignore rules without also requesting hidden files.',['--no-ignore']),
 ('rg','Use case-insensitive search only when the pattern has no uppercase letters.',['--smart-case']),
 ('rg','Search every file type except the specified type.',['--type-not']),
 ('rg','Terminate printed filenames with NUL so names containing newlines remain unambiguous.',['--null']),
 ('rg','Treat NUL as the input record terminator instead of newline.',['--null-data']),
 ('rg','Show only whole-word matches, not occurrences within longer words.',['--word-regexp']),
 ('rg','Show matching lines with line numbers, not byte offsets or column numbers.',['--line-number']),
 ('rg','Count matching lines and include files with zero matches: select the flag that adds the zero counts to an existing --count invocation.',['--include-zero']),
 ('rg','After existing --ignore-case, explicitly restore case-sensitive matching.',['--case-sensitive']),
 ('tail','Keep watching this log by its filename even when it is rotated and replaced.',['-F']),
 ('tail','Print the lines in reverse order, without following future writes.',['-r']),
 ('rm','Stay on the current filesystem during recursive removal.',['-x']),
 ('rm','Guarantee deleted data on an SSD is physically unrecoverable.',['NONE']),
 ('cp','Atomically roll back the entire copy operation if any one file fails.',['NONE']),
 ('rg','Sort the results by semantic relevance to a natural-language question.',['NONE']),
]
COMMAND_CASES = [
 ('context','Someone suggested rm, but I only want to rename the file and keep its contents.',['mv']),
 ('context','I do not want to delete the populated folder; make an independent backup copy of it.',['cp','rsync']),
 ('context','Do not add a remote. Replace the push address of an existing Git remote.',['git remote set-url']),
 ('context','The file is called tail.txt, but I want its first ten lines.',['head']),
 ('context','The file is called head.txt, but I want to watch new lines being appended.',['tail']),
 ('context','Search source file contents for the literal string "delete all files".',['rg','grep']),
 ('unsupported','Recover a permanently deleted regular file from an SSD without a backup.',['NONE']),
 ('unsupported','Show recursively how much disk space each subdirectory consumes, not the directory entry size.',['NONE']),
 ('unsupported','Change the Unix ownership of a file to a different user.',['NONE']),
 ('ambiguous','Compress it.',['CLARIFY','gzip','brotli','zip','tar']),
 ('composition','Download an encrypted file and decrypt it to local plaintext.',['COMPOSE']),
 ('composition','List the contents of this tar.gz archive without unpacking it or creating temporary files.',['tar']),
]

def freeze():
    source=json.loads((bench.OUT/'fixture.json').read_text())
    commands=source['tests'][0]['options']
    flag_options={c['command']:c['options'] for c in source['tests'] if c['category']=='flag'}
    tests=[]
    for i,(command,query,expected) in enumerate(FLAG_CASES):
        options=flag_options[command]
        assert set(expected)<=options.keys(), (command,expected)
        tests.append(dict(id=f'stress-flag-{i:02}',category='flag',command=command,query=query,expected=expected,options=options))
    for i,(category,query,expected) in enumerate(COMMAND_CASES):
        tests.append(dict(id=f'stress-command-{i:02}',category=category,query=query,expected=expected,options=commands))
    source['tests']=tests
    directory=bench.OUT/'stress'
    directory.mkdir(exist_ok=True)
    encoded=json.dumps(source,ensure_ascii=False,indent=2)+'\n'
    with (directory/'fixture.json').open('x') as f:
        f.write(encoded)
    print('Frozen challenge cases:',len(tests),'sha256:',hashlib.sha256(encoded.encode()).hexdigest())

def run(key):
    bench.OUT=BASE_OUTPUT/'stress'
    bench.evaluate(key)

def repeat(key):
    root=BASE_OUTPUT
    source=json.loads((root/'fixture.json').read_text())
    # Fixed subset chosen to cover all slices and known equivalent alternatives.
    ids=[0,7,14,15,27,34,36,39,40,44,46,48,49,50,51,52,53,54,55,56,57,58,59,60,63,64,68,69,70,71,74,75]
    source['tests']=[source['tests'][i] for i in ids]
    directory=root/'reversed'
    directory.mkdir(exist_ok=True)
    with (directory/'fixture.json').open('x') as f:
        json.dump(source,f,ensure_ascii=False,indent=2)
    original=bench.request_for
    bench.request_for=lambda case:original(case,reverse=True)
    bench.OUT=directory
    try:
        bench.evaluate(key)
    finally:
        bench.request_for=original

if __name__=='__main__':
    freeze()
