#!/usr/bin/env python3
"""File an epic plus children from a JSON spec, then attach the children as real GitHub sub-issues.
Spec: {"repo": "o/r", "labels_create": [[name,color,desc]], "epic": {title, body, labels}, "children": [{key,title,body,labels}]}
Bodies may reference other children as {{key}} and the epic as {{epic}}; they are filled in after creation."""
import json, subprocess, sys, re

def gh(*args, input=None):
    r = subprocess.run(["gh", *args], input=input, capture_output=True, text=True)
    if r.returncode: sys.exit(f"gh {' '.join(args[:3])}: {r.stderr}")
    return r.stdout.strip()

spec = json.load(open(sys.argv[1]))
repo = spec["repo"]
for name, color, desc in spec.get("labels_create", []):
    subprocess.run(["gh", "label", "create", name, "-R", repo, "-c", color, "-d", desc, "-f"], capture_output=True)

import time
def create(title, body, labels):
    time.sleep(int(__import__("os").environ.get("PACE", "20")))  # pace writes: a burst got the account suspended on 2026-09-29
    args = ["issue", "create", "-R", repo, "-t", title, "-F", "-"]
    for l in labels: args += ["-l", l]
    url = gh(*args, input=body)
    return int(url.rsplit("/", 1)[1])

nums = {}
epic = spec["epic"]
nums["epic"] = create(epic["title"], "(body follows)", epic.get("labels", []))
for c in spec["children"]:
    nums[c["key"]] = create(c["title"], "(body follows)", c.get("labels", []))

fill = lambda b: re.sub(r"\{\{(\w+)\}\}", lambda m: f"#{nums[m.group(1)]}", b)
gh("issue", "edit", str(nums["epic"]), "-R", repo, "-F", "-", input=fill(epic["body"]))
for c in spec["children"]:
    gh("issue", "edit", str(nums[c["key"]]), "-R", repo, "-F", "-", input=fill(c["body"]))

owner, name = repo.split("/")
for c in spec["children"]:
    iid = gh("api", f"repos/{repo}/issues/{nums[c['key']]}", "--jq", ".id")
    gh("api", "-X", "POST", f"repos/{repo}/issues/{nums['epic']}/sub_issues", "-F", f"sub_issue_id={iid}")

print(json.dumps({k: f"https://github.com/{repo}/issues/{v}" for k, v in nums.items()}, indent=1))
