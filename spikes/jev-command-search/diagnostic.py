"""Reproduce the one response rejected by the initial distribution validator."""
import http.client
import json
import pathlib
import time
import evaluate

def run(key):
    root=evaluate.ROOT/'target/jev-command-search'
    case=next(x for x in json.loads((root/'stress/fixture.json').read_text())['tests'] if x['id']=='stress-flag-11')
    connection=http.client.HTTPSConnection('api.typesafe.ai',timeout=10)
    rows=[]
    for i in range(5):
        start=time.perf_counter()
        connection.request('POST','/v1/systemone',json.dumps(evaluate.request_for(case,reverse=True)).encode(),
                           {'Authorization':'Bearer '+key,'Content-Type':'application/json'})
        response=connection.getresponse()
        body=response.read(1_000_001)
        assert response.status==200 and len(body)<=1_000_000
        data=json.loads(body)
        answer=data['answers']['selection']
        row=dict(iteration=i,latency_ms=round((time.perf_counter()-start)*1000,2),
                 choice=answer['choice'],confidence=answer['confidence'],
                 sum_probabilities=sum(answer['probabilities'].values()),
                 missing_options=sorted(case['options'].keys()-answer['probabilities'].keys()),
                 extra_options=sorted(answer['probabilities'].keys()-case['options'].keys()),
                 answer=answer,usage=data['usage'],model=data['model'])
        rows.append(row)
        print({k:v for k,v in row.items() if k not in ('answer','usage')},flush=True)
    connection.close()
    with (root/'diagnostic.json').open('x') as f:
        json.dump(rows,f,indent=2)
