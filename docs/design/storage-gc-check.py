#!/usr/bin/env python3
"""Explicit-state model checker for the M1c GC protocol.

An independent encoding of the transition system in `storage-gc.tla`, run here
because TLC needs Java (unavailable in this environment). It exhaustively
explores the bounded state space (BFS) and checks the safety + structural
invariants, printing a counterexample trace on the first violation. The TLA+
spec remains the spec-of-record; this is the runnable cross-check.

Model (clean-crash): block rows (refcount, available) keyed by (hash, gen);
files on "disk"; bound objects as a per-block multiset; writers and a collector
stepping through their named steps; a crash that can fire between any two steps;
a reconciling audit. Scale the model with env vars:

    NH=1 NW=2 MG=1 MAXREF=3 MAXOBJ=2 python3 storage-gc-check.py
    (NH hashes, NW writers, generations 0..MG, refcount/object caps)
"""
import os
from collections import deque

NH = int(os.environ.get('NH', '1'))
NW = int(os.environ.get('NW', '2'))
MG = int(os.environ.get('MG', '1'))
MAX_REF = int(os.environ.get('MAXREF', '3'))
MAX_OBJ = int(os.environ.get('MAXOBJ', '2'))

HASHES = [f'h{i}' for i in range(NH)]
WRITERS = [f'w{i}' for i in range(NW)]
GENS = list(range(MG + 1))
PH = HASHES[0]                       # placeholder hash for idle phases


def init():
    return {'rows': {}, 'files': frozenset(), 'objects': {},
            'wpc': {w: ('idle', PH, 0) for w in WRITERS}, 'gpc': ('idle', PH, 0)}


def clone(s):
    return {'rows': dict(s['rows']), 'files': s['files'],
            'objects': dict(s['objects']), 'wpc': dict(s['wpc']), 'gpc': s['gpc']}


def canon(s):
    rows = tuple(sorted((b, rc, av) for b, (rc, av) in s['rows'].items()))
    files = tuple(sorted(s['files']))
    objs = tuple(sorted((b, c) for b, c in s['objects'].items() if c > 0))
    wpc = tuple(sorted((w,) + ph for w, ph in s['wpc'].items()))
    return (rows, files, objs, wpc, s['gpc'])


def avail_gens(s, h):
    return [g for g in GENS if (h, g) in s['rows'] and s['rows'][(h, g)][1]]


def successors(s):
    out = []
    # RESERVE(w, h): reserve an available generation, else go to MATERIALIZE.
    for w in WRITERS:
        if s['wpc'][w][0] == 'idle':
            for h in HASHES:
                av = avail_gens(s, h)
                if av:
                    for g in av:
                        rc, a = s['rows'][(h, g)]
                        if rc < MAX_REF:
                            ns = clone(s); ns['rows'][(h, g)] = (rc + 1, a)
                            ns['wpc'][w] = ('reserved', h, g)
                            out.append((f'Reserve({w})->resv(g{g})', ns))
                else:
                    ns = clone(s); ns['wpc'][w] = ('materialize', h, 0)
                    out.append((f'Reserve({w})->materialize', ns))
    # MATERIALIZE(w): re-check at execution time -- dedup onto an available
    # generation if one now exists, else mint a fresh available generation.
    for w in WRITERS:
        ph, h, g = s['wpc'][w]
        if ph == 'materialize':
            av = avail_gens(s, h)
            if av:
                for gg in av:
                    rc, a = s['rows'][(h, gg)]
                    if rc < MAX_REF:
                        ns = clone(s); ns['rows'][(h, gg)] = (rc + 1, a)
                        ns['wpc'][w] = ('reserved', h, gg)
                        out.append((f'Materialize({w})->dedup(g{gg})', ns))
            else:
                for gg in GENS:
                    if (h, gg) not in s['rows']:
                        ns = clone(s); ns['rows'][(h, gg)] = (1, True)
                        ns['files'] = ns['files'] | {(h, gg)}
                        ns['wpc'][w] = ('reserved', h, gg)
                        out.append((f'Materialize({w})->mint(g{gg})', ns))
    # BIND: hand the reservation off to a bound object (refcount kept).
    for w in WRITERS:
        ph, h, g = s['wpc'][w]
        if ph == 'reserved':
            b = (h, g); c = s['objects'].get(b, 0)
            if c < MAX_OBJ:
                ns = clone(s); ns['objects'][b] = c + 1; ns['wpc'][w] = ('idle', PH, 0)
                out.append((f'Bind({w})', ns))
    # RELEASE: failed put gives the reservation back.
    for w in WRITERS:
        ph, h, g = s['wpc'][w]
        if ph == 'reserved':
            b = (h, g); rc, a = s['rows'][b]
            ns = clone(s); ns['rows'][b] = (rc - 1, a); ns['wpc'][w] = ('idle', PH, 0)
            out.append((f'Release({w})', ns))
    # UNBIND: a name delete drops a bound reference.
    for b in list(s['objects'].keys()):
        if s['objects'].get(b, 0) > 0 and b in s['rows']:
            rc, a = s['rows'][b]
            ns = clone(s); ns['objects'][b] = s['objects'][b] - 1; ns['rows'][b] = (rc - 1, a)
            out.append((f'Unbind({b})', ns))
    # CLAIM / GcResume (collector adopts a dead or orphaned-claimed block).
    if s['gpc'][0] == 'idle':
        for h in HASHES:
            for g in GENS:
                if (h, g) in s['rows']:
                    rc, a = s['rows'][(h, g)]
                    if rc == 0 and a:
                        ns = clone(s); ns['rows'][(h, g)] = (0, False); ns['gpc'] = ('claimed', h, g)
                        out.append((f'Claim({h},g{g})', ns))
                    elif rc == 0 and not a:
                        ns = clone(s); ns['gpc'] = ('claimed', h, g)
                        out.append((f'GcResume({h},g{g})', ns))
    # UNLINK
    if s['gpc'][0] == 'claimed':
        _, h, g = s['gpc']
        ns = clone(s); ns['files'] = ns['files'] - {(h, g)}; ns['gpc'] = ('unlinked', h, g)
        out.append(('Unlink', ns))
    # DELETE_ROW
    if s['gpc'][0] == 'unlinked':
        _, h, g = s['gpc']
        ns = clone(s)
        if (h, g) in ns['rows']:
            del ns['rows'][(h, g)]
        ns['gpc'] = ('idle', PH, 0)
        out.append(('DeleteRow', ns))
    # RECONCILE: audit repairs a leaked reservation when no writer holds it.
    for b in list(s['rows'].keys()):
        rc, a = s['rows'][b]
        objc = s['objects'].get(b, 0)
        held = any(s['wpc'][w][0] == 'reserved' and (s['wpc'][w][1], s['wpc'][w][2]) == b
                   for w in WRITERS)
        if rc > objc and not held:
            ns = clone(s); ns['rows'][b] = (objc, a)
            out.append((f'Reconcile({b})', ns))
    # CRASH (clean): abandon in-flight steps; committed rows/files/objects survive.
    ns = clone(s); ns['wpc'] = {w: ('idle', PH, 0) for w in WRITERS}; ns['gpc'] = ('idle', PH, 0)
    out.append(('Crash', ns))
    return out


def invariants(s):
    bad = []
    rows, files, objs = s['rows'], s['files'], s['objects']
    for b, c in objs.items():                       # S1 Safe (the forbidden state)
        if c > 0 and b not in files:
            bad.append(f'S1_Safe{b}')
    for b, (rc, a) in rows.items():                 # I2 LiveHasFile
        if rc > 0 and b not in files:
            bad.append(f'I2_LiveHasFile{b}')
    for h in HASHES:                                # I1 OneAvailable
        if sum(1 for g in GENS if (h, g) in rows and rows[(h, g)][1]) > 1:
            bad.append(f'I1_OneAvailable({h})')
    if s['gpc'][0] in ('claimed', 'unlinked'):      # I3 ClaimedHasNoRefs
        _, h, g = s['gpc']
        if (h, g) in rows and rows[(h, g)][0] != 0:
            bad.append('I3_ClaimedHasNoRefs')
    for b, (rc, a) in rows.items():                 # I4 RefcountDominates + nonneg
        if rc < objs.get(b, 0):
            bad.append(f'I4_RefcountDominates{b}')
        if rc < 0:
            bad.append(f'NegRefcount{b}')
    return bad


def check():
    start = init()
    visited = {canon(start): None}
    q = deque([start])
    n = 0
    while q:
        s = q.popleft(); n += 1
        bad = invariants(s)
        if bad:
            trace, cur = [], canon(s)
            while visited[cur] is not None:
                parent, act = visited[cur]
                trace.append(act); cur = parent
            trace.reverse()
            print(f"VIOLATION ({bad}) after exploring {n} states.\nCounterexample:")
            for a in trace:
                print("   ", a)
            return False
        for act, ns in successors(s):
            c = canon(ns)
            if c not in visited:
                visited[c] = (canon(s), act)
                q.append(ns)
    print(f"OK ({NH}h/{NW}w/gen0..{MG}): {len(visited)} states, no violations "
          "[S1,I1,I2,I3,I4,nonneg].")
    return True


if __name__ == '__main__':
    import sys
    sys.exit(0 if check() else 1)
