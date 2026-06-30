#!/usr/bin/env python3
"""Explicit-state model checker for the M1c GC protocol.

Independent encoding of storage_gc.tla, run here because TLC needs Java. Two
checks (MODE):

  safety   -- BFS over all reachable states, checking S1, I1-I4 and
              non-negativity. CRASH selects the crash semantics:
                clean      -- committed state survives, in-flight abandoned;
                unfsynced  -- a crash also drops files not yet fsynced.
              SYNC selects whether the block file is fsync'd (durable) before
              its index row is committed.

  liveness -- fair-cycle detection over the crash-free transition graph, under
              weak fairness on the progress actions, checking that the collector
              and writers never get stuck and that reclaim makes progress.

Env: NH NW MG MAXREF MAXOBJ (size); CRASH=clean|unfsynced; SYNC=1|0;
MODE=safety|liveness.
"""
import os
import sys
from collections import deque

NH = int(os.environ.get('NH', '1'))
NW = int(os.environ.get('NW', '2'))
MG = int(os.environ.get('MG', '1'))
MAX_REF = int(os.environ.get('MAXREF', '3'))
MAX_OBJ = int(os.environ.get('MAXOBJ', '2'))
CRASH = os.environ.get('CRASH', 'clean')          # clean | unfsynced
SYNC = os.environ.get('SYNC', '1') == '1'         # fsync block file before INSERT
MODE = os.environ.get('MODE', 'safety')           # safety | liveness

HASHES = [f'h{i}' for i in range(NH)]
WRITERS = [f'w{i}' for i in range(NW)]
GENS = list(range(MG + 1))
PH = HASHES[0]
WF = ['Materialize', 'Bind', 'Release', 'Claim', 'GcResume', 'Unlink', 'DeleteRow', 'Reconcile']


def init():
    return {'rows': {}, 'files': {}, 'objects': {},
            'wpc': {w: ('idle', PH, 0) for w in WRITERS}, 'gpc': ('idle', PH, 0)}


def clone(s):
    return {'rows': dict(s['rows']), 'files': dict(s['files']),
            'objects': dict(s['objects']), 'wpc': dict(s['wpc']), 'gpc': s['gpc']}


def canon(s):
    return (tuple(sorted((b, rc, av) for b, (rc, av) in s['rows'].items())),
            tuple(sorted((b, sy) for b, sy in s['files'].items())),
            tuple(sorted((b, c) for b, c in s['objects'].items() if c > 0)),
            tuple(sorted((w,) + ph for w, ph in s['wpc'].items())),
            s['gpc'])


def avail(s, h):
    return [g for g in GENS if (h, g) in s['rows'] and s['rows'][(h, g)][1]]


def successors(s):
    out = []
    for w in WRITERS:                                  # RESERVE
        if s['wpc'][w][0] == 'idle':
            for h in HASHES:
                av = avail(s, h)
                if av:
                    for g in av:
                        rc, a = s['rows'][(h, g)]
                        if rc < MAX_REF:
                            ns = clone(s); ns['rows'][(h, g)] = (rc + 1, a)
                            ns['wpc'][w] = ('reserved', h, g)
                            out.append((f'Reserve({w})', ns))
                else:
                    ns = clone(s); ns['wpc'][w] = ('materialize', h, 0)
                    out.append((f'Reserve({w})', ns))
    for w in WRITERS:                                  # MATERIALIZE (dedup or mint)
        ph, h, g = s['wpc'][w]
        if ph == 'materialize':
            av = avail(s, h)
            if av:
                for gg in av:
                    rc, a = s['rows'][(h, gg)]
                    if rc < MAX_REF:
                        ns = clone(s); ns['rows'][(h, gg)] = (rc + 1, a)
                        ns['wpc'][w] = ('reserved', h, gg)
                        out.append((f'Materialize({w})', ns))
            else:
                for gg in GENS:
                    if (h, gg) not in s['rows']:
                        ns = clone(s); ns['rows'][(h, gg)] = (1, True)
                        ns['files'][(h, gg)] = SYNC      # synced iff fsync-before-insert
                        ns['wpc'][w] = ('reserved', h, gg)
                        out.append((f'Materialize({w})', ns))
    for w in WRITERS:                                  # BIND / RELEASE
        ph, h, g = s['wpc'][w]
        if ph == 'reserved':
            b = (h, g)
            if s['objects'].get(b, 0) < MAX_OBJ:
                ns = clone(s); ns['objects'][b] = s['objects'].get(b, 0) + 1
                ns['wpc'][w] = ('idle', PH, 0)
                out.append((f'Bind({w})', ns))
            rc, a = s['rows'][b]
            ns = clone(s); ns['rows'][b] = (rc - 1, a); ns['wpc'][w] = ('idle', PH, 0)
            out.append((f'Release({w})', ns))
    for b in list(s['objects'].keys()):                # UNBIND
        if s['objects'].get(b, 0) > 0 and b in s['rows']:
            rc, a = s['rows'][b]
            ns = clone(s); ns['objects'][b] -= 1; ns['rows'][b] = (rc - 1, a)
            out.append(('Unbind', ns))
    if s['gpc'][0] == 'idle':                          # CLAIM / GcResume
        for h in HASHES:
            for g in GENS:
                if (h, g) in s['rows']:
                    rc, a = s['rows'][(h, g)]
                    if rc == 0 and a:
                        ns = clone(s); ns['rows'][(h, g)] = (0, False); ns['gpc'] = ('claimed', h, g)
                        out.append(('Claim', ns))
                    elif rc == 0 and not a:
                        ns = clone(s); ns['gpc'] = ('claimed', h, g)
                        out.append(('GcResume', ns))
    if s['gpc'][0] == 'claimed':                       # UNLINK
        _, h, g = s['gpc']
        ns = clone(s); ns['files'].pop((h, g), None); ns['gpc'] = ('unlinked', h, g)
        out.append(('Unlink', ns))
    if s['gpc'][0] == 'unlinked':                      # DELETE_ROW
        _, h, g = s['gpc']
        ns = clone(s); ns['rows'].pop((h, g), None); ns['gpc'] = ('idle', PH, 0)
        out.append(('DeleteRow', ns))
    for b in list(s['rows'].keys()):                   # RECONCILE
        rc, a = s['rows'][b]
        objc = s['objects'].get(b, 0)
        held = any(s['wpc'][w][0] == 'reserved' and (s['wpc'][w][1], s['wpc'][w][2]) == b
                   for w in WRITERS)
        if rc > objc and not held:
            ns = clone(s); ns['rows'][b] = (objc, a)
            out.append(('Reconcile', ns))
    ns = clone(s)                                      # CRASH
    ns['wpc'] = {w: ('idle', PH, 0) for w in WRITERS}; ns['gpc'] = ('idle', PH, 0)
    if CRASH == 'unfsynced':
        ns['files'] = {b: sy for b, sy in ns['files'].items() if sy}   # drop unsynced files
    out.append(('Crash', ns))
    return out


def invariants(s):
    bad = []
    rows, files, objs = s['rows'], s['files'], s['objects']
    for b, c in objs.items():
        if c > 0 and b not in files:
            bad.append(f'S1_Safe{b}')
    for b, (rc, a) in rows.items():
        if rc > 0 and b not in files:
            bad.append(f'I2_LiveHasFile{b}')
    for h in HASHES:
        if sum(1 for g in GENS if (h, g) in rows and rows[(h, g)][1]) > 1:
            bad.append(f'I1({h})')
    if s['gpc'][0] in ('claimed', 'unlinked'):
        _, h, g = s['gpc']
        if (h, g) in rows and rows[(h, g)][0] != 0:
            bad.append('I3')
    for b, (rc, a) in rows.items():
        if rc < objs.get(b, 0):
            bad.append(f'I4{b}')
        if rc < 0:
            bad.append(f'Neg{b}')
    return bad


def check_safety():
    start = init()
    visited = {canon(start): None}
    q = deque([start]); n = 0
    while q:
        s = q.popleft(); n += 1
        bad = invariants(s)
        if bad:
            tr, cur = [], canon(s)
            while visited[cur] is not None:
                p, act = visited[cur]; tr.append(act); cur = p
            tr.reverse()
            print(f"  VIOLATION ({bad}) after {n} states:")
            for a in tr:
                print("     ", a)
            return False
        for act, ns in successors(s):
            c = canon(ns)
            if c not in visited:
                visited[c] = (canon(s), act); q.append(ns)
    print(f"  OK: {len(visited)} states, no violations [S1,I1,I2,I3,I4,nonneg].")
    return True


def tarjan(nodes, adj):
    index, low, onstack, stack, idx, sccs = {}, {}, {}, [], [0], []
    sys.setrecursionlimit(5 * 10**6)

    def strong(v):
        index[v] = low[v] = idx[0]; idx[0] += 1; stack.append(v); onstack[v] = True
        for _, w in adj.get(v, []):
            if w not in index:
                strong(w); low[v] = min(low[v], low[w])
            elif onstack.get(w):
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop(); onstack[w] = False; comp.append(w)
                if w == v:
                    break
            sccs.append(comp)
    for v in nodes:
        if v not in index:
            strong(v)
    return sccs


def check_liveness():
    # reachable states (with crash, for reachability) + crash-free edges
    start = init()
    states = {canon(start): start}
    q = deque([start])
    while q:
        s = q.popleft()
        for act, ns in successors(s):
            c = canon(ns)
            if c not in states:
                states[c] = ns; q.append(ns)
    nodes = list(states.keys())
    adj = {c: [] for c in nodes}
    enabled = {}
    for c, s in states.items():
        en = set()
        for act, ns in successors(s):
            pref = act.split('(')[0]
            if pref != 'Crash':
                adj[c].append((act, canon(ns)))
                if pref in WF:
                    pass
            if pref in WF:
                en.add(pref)
        enabled[c] = en
    sccs = tarjan(nodes, adj)
    viol = []
    for comp in sccs:
        cs = set(comp)
        if not (len(comp) > 1 or any(w == comp[0] for _, w in adj[comp[0]])):
            continue                                   # no cycle in this SCC
        taken = set()
        for v in comp:
            for a, w in adj[v]:
                if w in cs:
                    taken.add(a.split('(')[0])
        fair = all(p in taken or any(p not in enabled[v] for v in comp) for p in WF)
        if not fair:
            continue
        st = [states[c] for c in comp]
        if all(x['gpc'][0] != 'idle' for x in st):
            viol.append('GC-progress: collector stuck non-idle on a fair cycle')
            continue
        starved = next((w for w in WRITERS if all(x['wpc'][w][0] != 'idle' for x in st)), None)
        if starved:
            viol.append(f'writer-progress: {starved} stuck non-idle on a fair cycle')
            continue
        if 'DeleteRow' not in taken:
            dead = next(((h, g) for h in HASHES for g in GENS
                         if all((h, g) in x['rows'] and x['rows'][(h, g)][0] == 0 for x in st)), None)
            if dead:
                viol.append(f'reclaim-progress: {dead} stays dead, never reclaimed on a fair cycle')
    if viol:
        print(f"  LIVENESS VIOLATION(S): {len(viol)}")
        for v in viol[:5]:
            print("     ", v)
        return False
    print(f"  OK: {len(nodes)} states, no fair-cycle violations "
          "[GC-progress, writer-progress, reclaim-progress].")
    return True


if __name__ == '__main__':
    print(f"[{NH}h/{NW}w/gen0..{MG}  MODE={MODE} CRASH={CRASH} SYNC={SYNC}]")
    ok = check_liveness() if MODE == 'liveness' else check_safety()
    sys.exit(0 if ok else 1)
