#!/usr/bin/env python3
"""Step 1 — build per-PR dataset from git refs: agent-authored PRs + corrective commits.

Prereqs: a clone at $FQ_REPO (default ./factor-q) with PR refs fetched:
  git fetch origin '+refs/pull/*/head:refs/remotes/pr/*'
Writes: pr_dataset.json (cwd).
"""
import subprocess, json, re, sys, os
from collections import defaultdict

REPO = os.environ.get("FQ_REPO", "factor-q")
AGENT_EMAIL = re.compile(r"@(agents\.invalid|factor-q\.local)$")
FIX_MSG = re.compile(r"\b(fix|correct|address|revert|repair|missed|regression|broken|typo|bug)\b", re.I)

def git(*args):
    return subprocess.run(["git", "-C", REPO] + list(args),
                          capture_output=True, text=True).stdout

def commits_between(base, head):
    out = git("log", "--reverse", "--format=%H%x01%an%x01%ae%x01%aI%x01%s", f"{base}..{head}")
    rows = []
    for line in out.strip().splitlines():
        if not line: continue
        sha, an, ae, date, subj = line.split("\x01")
        rows.append({"sha": sha[:10], "author": an, "email": ae, "date": date,
                     "subject": subj, "is_agent": bool(AGENT_EMAIL.search(ae))})
    return rows

def files_of(sha):
    return set(git("show", "--name-only", "--format=", sha).strip().splitlines())

# enumerate PR refs
refs = git("for-each-ref", "refs/remotes/pr", "--format=%(refname:short) %(objectname)").strip().splitlines()
prs = []
for line in refs:
    ref, head = line.split()
    num = int(ref.split("/")[-1])
    base = git("merge-base", "origin/main", head).strip()
    if not base: continue
    merged = subprocess.run(["git", "-C", REPO, "merge-base", "--is-ancestor", head, "origin/main"]).returncode == 0
    commits = commits_between(base, head)
    if not commits: continue
    agent_authored = commits[0]["is_agent"]
    # within-PR corrections: human commits after an agent commit
    within = []
    seen_agent = False
    for c in commits:
        if c["is_agent"]: seen_agent = True
        elif seen_agent: within.append(c)
    prs.append({"pr": num, "head": head[:10], "merged": merged,
                "agent_authored": agent_authored,
                "agent_id": commits[0]["email"].split("@")[0] if agent_authored else None,
                "n_commits": len(commits), "commits": commits,
                "within_pr_corrections": within})

prs.sort(key=lambda p: p["pr"])

# post-merge corrections: for merged agent PRs, scan main after last PR commit
main_log = git("log", "--reverse", "--first-parent", "--format=%H%x01%ae%x01%aI%x01%s", "origin/main").strip().splitlines()
main_commits = []
for line in main_log:
    sha, ae, date, subj = line.split("\x01")
    main_commits.append({"sha": sha, "email": ae, "date": date, "subject": subj,
                        "is_agent": bool(AGENT_EMAIL.search(ae))})
main_index = {c["sha"][:10]: i for i, c in enumerate(main_commits)}

for p in prs:
    p["post_merge_corrections"] = []
    if not (p["merged"] and p["agent_authored"]): continue
    agent_shas = [c["sha"] for c in p["commits"] if c["is_agent"]]
    if not agent_shas: continue
    last = agent_shas[-1]
    if last not in main_index: continue  # squashed/rewritten
    pr_files = set()
    for s in agent_shas: pr_files |= files_of(s)
    i = main_index[last]
    for c in main_commits[i+1 : i+80]:  # look-ahead window on main
        if c["is_agent"]: continue
        f = files_of(c["sha"])
        overlap = pr_files & f
        if overlap and FIX_MSG.search(c["subject"]):
            p["post_merge_corrections"].append({
                "sha": c["sha"][:10], "date": c["date"], "subject": c["subject"],
                "overlap": sorted(overlap)[:6]})

with open("pr_dataset.json", "w") as fh:
    json.dump(prs, fh, indent=1)

# summary
agent_prs = [p for p in prs if p["agent_authored"]]
print(f"PRs total: {len(prs)} | agent-authored: {len(agent_prs)} "
      f"(merged: {sum(1 for p in agent_prs if p['merged'])})")
print(f"agent PRs w/ within-PR human corrections: {sum(1 for p in agent_prs if p['within_pr_corrections'])}")
print(f"agent PRs w/ post-merge corrective commits: {sum(1 for p in agent_prs if p['post_merge_corrections'])}")
print("\n#PR  merged  agent-id       commits  within  post   subject-of-first-commit")
for p in agent_prs:
    print(f"{p['pr']:>4}  {'y' if p['merged'] else 'n':<6}  {(p['agent_id'] or '')[:13]:<13}  "
          f"{p['n_commits']:>3}      {len(p['within_pr_corrections']):>2}    {len(p['post_merge_corrections']):>2}   "
          f"{p['commits'][0]['subject'][:60]}")
