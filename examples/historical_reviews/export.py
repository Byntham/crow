"""Export reports and runtime receipts, excluding provider transcripts and credentials."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('input', type=Path)
parser.add_argument('output', type=Path)
args = parser.parse_args()
cases = []
for spec in json.loads(Path(__file__).with_name('cases.json').read_text()):
    directory = args.input / spec['id']
    if not (directory / 'source.json').exists():
        continue
    source = json.loads((directory / 'source.json').read_text())
    case = dict(spec, url=f'https://github.com/{spec["repo"]}/pull/{spec["number"]}',
                base=source['base'], head=source['head'], attempts=[])
    for state in sorted(directory.iterdir()):
        events = state / 'reviews' / spec['id'] / 'events.jsonl'
        if not events.exists():
            continue
        report = state / 'result.json'
        calls = {}
        viewed = []
        for line in events.read_text().splitlines():
            event = json.loads(line)
            item = event.get('item', {})
            if event['type'] != 'item.completed' or item.get('type') != 'mcp_tool_call':
                continue
            name = item['tool']
            calls[name] = calls.get(name, 0) + 1
            if name == 'read_artifact':
                result = item.get('result') or {}
                viewed.append(dict(arguments=item['arguments'], status=item['status'],
                                   imageReturned=any(c['type'] == 'image' for c in result.get('content', []))))
        receipts = [json.loads(p.read_text()) for p in sorted(events.parent.joinpath('experiments').glob('*.json'))]
        receipts.sort(key=lambda r: r.get('startedAt', ''))
        case['attempts'].append(dict(name=state.name,
            replay=json.loads((state / 'replay.json').read_text()) if (state / 'replay.json').exists() else None,
            report=json.loads(report.read_text()) if report.exists() else None,
            error=(state / 'error.txt').read_text() if (state / 'error.txt').exists() else None,
            toolCalls=calls, viewedArtifacts=viewed, receipts=receipts))
    case['independentChecks'] = [json.loads(p.read_text()) for p in sorted(directory.joinpath('verification/experiments').glob('*.json'))]
    cases.append(case)
args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(cases, indent=2) + '\n')
print(f'Exported {len(cases)} comparisons to {args.output}')
