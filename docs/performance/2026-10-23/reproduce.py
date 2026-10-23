#!/usr/bin/env python3
"""Run the unchanged compare binaries in baseline/candidate/candidate/baseline order."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import time


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--traces', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--cpu', type=int)
    parser.add_argument('--reps', type=int, default=100)
    parser.add_argument('--reference-drift', type=float, default=.05)
    args = parser.parse_args()
    if args.reps < 1 or not 0 < args.reference_drift < 1:
        parser.error('reps must be positive and reference-drift must be between zero and one')
    baseline, candidate = args.baseline.resolve(), args.candidate.resolve()
    traces = sorted(args.traces.resolve().glob('*.json'))
    if not traces:
        parser.error('no JSON histories found')
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=False)
    if args.cpu is not None:
        os.sched_setaffinity(0, {args.cpu})
    manifest = dict(reps=args.reps, cpu=args.cpu, reference_drift=args.reference_drift,
                    binaries={str(p): sha256(p) for p in [baseline, candidate]},
                    traces={p.name: sha256(p) for p in traces})
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    runs = {}
    for label, binary in [('baseline', baseline), ('candidate', candidate),
                          ('candidate-repeat', candidate), ('baseline-repeat', baseline)]:
        destination = output / (label + '.json')
        command = [str(binary), '--reps', str(args.reps), '--without', 'yrs',
                   '--without', 'loro', '--without', 'automerge', '--json', str(destination)]
        command += [str(p) for p in traces]
        print('Running ' + label, flush=True)
        start = time.perf_counter()
        with (output / (label + '.log')).open('w') as log:
            subprocess.run(command, cwd=output, stdout=log, stderr=subprocess.STDOUT, check=True)
        rows = json.loads(destination.read_text())
        if {r['trace'] for r in rows} != {p.stem for p in traces}:
            raise RuntimeError('Comparator did not return the complete input suite')
        if not all(r['same']['strider'] and r['same']['dt'] for r in rows):
            raise RuntimeError('A result differs from the diamond-types text oracle')
        runs[label] = {r['trace']: r for r in rows}
        (output / (label + '-execution.json')).write_text(json.dumps(
            dict(wall_seconds=time.perf_counter() - start, command=command), indent=2) + '\n')
    def compare(base, result):
        ratios = {name: base[name]['merge_ms']['strider'] / row['merge_ms']['strider']
                  for name, row in result.items()}
        control = {name: base[name]['merge_ms']['dt'] / row['merge_ms']['dt']
                   for name, row in result.items()}
        return dict(S=math.exp(statistics.mean(math.log(r) for r in ratios.values())),
                    per_trace_speedup=ratios, unchanged_reference_ratios=control,
                    control_passed=all(1 - args.reference_drift <= r <= 1 + args.reference_drift
                                       for r in control.values()))
    first = compare(runs['baseline'], runs['candidate'])
    repeat = compare(runs['baseline-repeat'], runs['candidate-repeat'])
    result = dict(first=first, repeat=repeat,
                  accepted=first['control_passed'] and repeat['control_passed'])
    (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))
    if not result['accepted']:
        raise SystemExit('Reference drift exceeded the limit; retain this attempt and repeat in a new output directory')


if __name__ == '__main__':
    main()
