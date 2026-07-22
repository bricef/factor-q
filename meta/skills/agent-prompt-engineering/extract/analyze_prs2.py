#!/usr/bin/env python3
"""Step 2 — landing/rejection mapping via unique author emails + correction details.

Prereqs: step 1 output (pr_dataset.json) in cwd; same $FQ_REPO clone.
Writes: agent_pr_report.json (cwd) and prints the summary tables.
Note: post-merge correction detection is a file-overlap+message heuristic —
treat as candidates and verify semantically before counting.
"""
import subprocess, json, re, os
from collections import defaultdict

REPO = os.environ.get("FQ_REPO", "factor-q")
AGENT_EMAIL = re.compile(r"@(agents\.invalid|factor-q\.local)$")
FIX_MSG = re.compile(r"\b(fix|correct|address|revert|repair|missed|regression|broken|hotfix|follow-?up)\b", re.I)
ISSUE_REF = re.compile(r"#(\d+)")

def git(*args):
    return subprocess.run(["git", "-C", REPO] + list(args), capture_output=True, text=True).stdout

prs = json.load(open("pr_dataset.json"))

# main history with full bodies
main_raw = git("log", "--reverse", "--first-parent", "--format=%H%x01%ae%x01%aI%x01%s%x01%b%x02", "origin/main")
main_commits = []
for chunk in main_raw.split("\x02"):
    chunk = chunk.strip("\n")
    if not chunk: continue
    sha, ae, date, subj, body = (chunk.split("\x01") + [""])[:5]
    main_commits.append({"sha": sha, "email": ae, "date": date, "subject": subj, "body": body,
                         "is_agent": bool(AGENT_EMAIL.search(ae))})
by_email = defaultdict(list)
by_subj = defaultdict(list)
for i, c in enumerate(main_commits):
    by_email[c["email"]].append(i)
    by_subj[c["subject"]].append(i)

def files_of(sha):
    return set(git("show", "--name-only", "--format=", sha).strip().splitlines())

report = []
for p in prs:
    if not p["agent_authored"]: continue
    agent_commits = [c for c in p["commits"] if c["is_agent"]]
    email = agent_commits[0]["email"]
    # landing: unique email (uuid cohort) or subject match (older cohort)
    land_idx = None
    if "+" in email and by_email.get(email):
        land_idx = by_email[email][0]
    else:
        for c in agent_commits:
            if by_subj.get(c["subject"]):
                land_idx = by_subj[c["subject"]][0]; break
    landed = land_idx is not None
    # post-merge corrections on main
    post = []
    if landed:
        land_sha = main_commits[land_idx]["sha"]
        pr_files = set()
        for c in agent_commits: pr_files |= files_of(c["sha"])
        for c in main_commits[land_idx+1 : land_idx+120]:
            if c["is_agent"]: continue
            ov = pr_files & files_of(c["sha"])
            if ov and FIX_MSG.search(c["subject"]):
                post.append({"sha": c["sha"][:10], "date": c["date"][:10],
                             "subject": c["subject"], "overlap": sorted(ov)[:5]})
    # within-PR correction details incl. trailers
    within = []
    for c in p["within_pr_corrections"]:
        body = git("log", "-1", "--format=%b", c["sha"])
        coauth = [l.strip() for l in body.splitlines() if "co-authored-by" in l.lower()]
        within.append({**c, "coauthors": coauth})
    issues = sorted({int(m) for c in p["commits"]
                     for m in ISSUE_REF.findall(c["subject"] + " " + git("log","-1","--format=%b",c["sha"]))})
    report.append({"pr": p["pr"], "agent_id": p["agent_id"], "landed": landed,
                   "land_sha": main_commits[land_idx]["sha"][:10] if landed else None,
                   "land_date": main_commits[land_idx]["date"][:10] if landed else None,
                   "n_commits": p["n_commits"], "issues_referenced": issues,
                   "first_subject": p["commits"][0]["subject"],
                   "within_pr_corrections": within, "post_merge_corrections": post[:8]})

json.dump(report, open("agent_pr_report.json", "w"), indent=1)

landed = [r for r in report if r["landed"]]
rejected = [r for r in report if not r["landed"]]
w = [r for r in report if r["within_pr_corrections"]]
pm = [r for r in landed if r["post_merge_corrections"]]
clean = [r for r in landed if not r["within_pr_corrections"] and not r["post_merge_corrections"]]
print(f"agent PRs: {len(report)} | landed: {len(landed)} | rejected/unlanded: {len(rejected)}")
print(f"needed within-PR correction: {len(w)} | needed post-merge correction: {len(pm)} | landed clean: {len(clean)}")
print("\nREJECTED PRs:")
for r in rejected: print(f"  #{r['pr']:>3} {r['first_subject'][:70]}")
print("\nPOST-MERGE-CORRECTED PRs:")
for r in pm:
    print(f"  #{r['pr']:>3} {r['first_subject'][:64]}")
    for c in r["post_merge_corrections"][:3]:
        print(f"        -> {c['date']} {c['subject'][:66]}")
