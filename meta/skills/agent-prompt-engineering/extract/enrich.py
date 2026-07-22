#!/usr/bin/env python3
"""Step 3 — one-shot GitHub API enrichment (issues, PR states, reviews, label timelines).
Usage: GH_TOKEN=<fine-grained read-only PAT> python3 enrich.py
Scope needed: bricef/factor-q — Metadata, Contents, Issues, Pull requests (all read-only).
Writes: issues_all.json, reviews/<n>_{reviews,inline,convo}.json, timelines/<n>.json
"""
import json, os, sys, time, urllib.request, pathlib

TOK = os.environ.get("GH_TOKEN")
if not TOK:
    sys.exit("GH_TOKEN not set")
H = {"Accept": "application/vnd.github+json", "User-Agent": "fq-analysis",
     "Authorization": f"Bearer {TOK}", "X-GitHub-Api-Version": "2022-11-28"}
BASE = f"https://api.github.com/repos/{os.environ.get('GH_REPO', 'bricef/factor-q')}"
calls = 0

def api(url):
    global calls
    calls += 1
    req = urllib.request.Request(url, headers=H)
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req) as r:
                return json.load(r)
        except urllib.error.HTTPError as e:
            if e.code in (403, 429) and attempt < 2:
                time.sleep(int(e.headers.get("Retry-After", 15))); continue
            if e.code == 404:
                return None
            raise

# 1. every issue + PR, with bodies, states, labels
items, page = [], 1
while True:
    batch = api(f"{BASE}/issues?state=all&per_page=100&page={page}")
    items += batch
    if len(batch) < 100: break
    page += 1
json.dump(items, open("issues_all.json", "w"))
prs = {i["number"] for i in items if "pull_request" in i}
print(f"listing: {len(items)} items ({len(prs)} PRs) in {calls} calls")

# 2. agent PR numbers from the git-derived report
report = json.load(open("agent_pr_report.json"))
agent_prs = [r["pr"] for r in report]
issue_refs = sorted({n for r in report for n in r["issues_referenced"]})

pathlib.Path("reviews").mkdir(exist_ok=True)
for n in agent_prs:
    for kind, path in (("reviews", f"pulls/{n}/reviews"),
                       ("inline",  f"pulls/{n}/comments"),
                       ("convo",   f"issues/{n}/comments")):
        d = api(f"{BASE}/{path}?per_page=100")
        json.dump(d or [], open(f"reviews/{n}_{kind}.json", "w"))
print(f"reviews fetched for {len(agent_prs)} PRs (calls so far: {calls})")

# 3. label timelines for referenced issues + unlanded PRs (fleet:refined at dispatch?)
pathlib.Path("timelines").mkdir(exist_ok=True)
for n in sorted(set(issue_refs) | {r["pr"] for r in report if not r["landed"]}):
    d = api(f"{BASE}/issues/{n}/timeline?per_page=100")
    json.dump(d or [], open(f"timelines/{n}.json", "w"))
print(f"done: {calls} API calls total")
