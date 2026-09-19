"""Summarize the frozen Jev run and a completed Luna CLI run without inference."""
import argparse
import json
import math
from pathlib import Path
import statistics


def distribution(values):
    ordered = sorted(values)
    return dict(p50=statistics.median(ordered), p95=ordered[math.ceil(len(ordered)*.95)-1],
                minimum=ordered[0], maximum=ordered[-1])


def compare(directory):
    evidence = json.loads(Path(__file__).with_name('evidence.json').read_text())
    luna = [json.loads(line) for line in (directory/'results.jsonl').read_text().splitlines()]
    jev = []
    for group in evidence['sets']:
        for original in group['results']:
            row = dict(original, set=group['name'])
            for correction in evidence['corrections']:
                if correction['set'] == group['name'] and correction['case'] == row['id']:
                    row['correct'] = row.get('choice') in correction['accepted_after_review']
            jev.append(row)
    assert len(luna) == len(jev) == 144, 'Require the complete primary experiment.'
    assert [(r['set'],r['id']) for r in luna] == [(r['set'],r['id']) for r in jev]
    report = {}
    for name, rows, field in [('jev',jev,'latency_ms'),('luna',luna,'process_ms')]:
        report[name] = dict(requests=len(rows), valid=sum('error' not in r for r in rows),
                            acceptable=sum(bool(r.get('correct')) for r in rows),
                            latency_ms=distribution([r[field] for r in rows]))
        report[name]['sets'] = {group:dict(total=sum(r['set']==group for r in rows),
            acceptable=sum(r['set']==group and bool(r.get('correct')) for r in rows))
            for group in ['initial','challenge','reversed']}
    usage = {key:sum(r.get('usage',{}).get(key,0) for r in luna)
             for key in ['input_tokens','cached_input_tokens','cache_write_input_tokens',
                         'output_tokens','reasoning_output_tokens']}
    assert all(type(n) is int and n>=0 for n in usage.values())
    assert usage['cached_input_tokens']<=usage['input_tokens']
    assert usage['cache_write_input_tokens']==0, 'Add cache-write pricing before reporting it.'
    luna_cost = ((usage['input_tokens']-usage['cached_input_tokens'])*.20
                 + usage['cached_input_tokens']*.02 + usage['output_tokens']*1.20)/1e6
    credits = ((usage['input_tokens']-usage['cached_input_tokens'])*5
               + usage['cached_input_tokens']*.5 + usage['output_tokens']*30)/1e6
    jev_tokens = sum(r.get('usage',{}).get('input_tokens',0) for r in jev)
    report['luna'].update(usage=usage, estimated_api_equivalent_usd=luna_cost,
        estimated_published_credits=credits,
        turn_ms=distribution([r['turn_ms'] for r in luna if 'turn_ms' in r]),
        outside_turn_ms=distribution([r['process_ms']-r['turn_ms'] for r in luna if 'turn_ms' in r]),
        warnings=sum(bool(r.get('warnings')) for r in luna))
    report['jev'].update(known_input_tokens=jev_tokens, known_estimated_usd=jev_tokens*.042/1e6)
    pairs = [(j,l) for j,l in zip(jev,luna) if j.get('usage') and l.get('usage')]
    def cost(row):
        u=row['usage']
        return ((u['input_tokens']-u['cached_input_tokens'])*.20
                +u['cached_input_tokens']*.02+u['output_tokens']*1.20)/1e6
    report['paired_cost'] = dict(cases=len(pairs),
        luna_usd=sum(cost(l) for j,l in pairs),
        jev_usd=sum(j['usage']['input_tokens']*.042/1e6 for j,l in pairs))
    report['paired_cost']['ratio_luna_over_jev'] = report['paired_cost']['luna_usd']/report['paired_cost']['jev_usd']
    report['median_workflow_latency_ratio'] = report['luna']['latency_ms']['p50']/report['jev']['latency_ms']['p50']
    report['disagreements'] = [dict(set=l['set'],id=l['id'],jev=j.get('choice'),luna=l.get('choice'),
                                  jev_acceptable=j.get('correct'),luna_acceptable=l.get('correct'))
                               for j,l in zip(jev,luna) if j.get('choice') != l.get('choice')]
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    args = parser.parse_args()
    print(json.dumps(compare(args.directory),ensure_ascii=False,indent=2))
