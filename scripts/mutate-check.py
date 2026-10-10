#!/usr/bin/env python3
"""Apply one textual mutation at a time to a source file, run the tests that
should catch it, report caught/MISSED, and restore the file.

usage: mutate.py FILE MUTATIONS.json FILTER [FILTER...]
MUTATIONS.json: [{"name":..., "old":..., "new":...}, ...]
"""
import json, pathlib, shutil, subprocess, sys

file = pathlib.Path(sys.argv[1])
mutations = json.loads(pathlib.Path(sys.argv[2]).read_text())
filters = sys.argv[3:]
backup = file.with_suffix(file.suffix + ".orig-mutate")
shutil.copy(file, backup)
missed = []
try:
    for m in mutations:
        original = backup.read_text()
        count = original.count(m["old"])
        if count != 1:
            print(f"SKIP {m['name']}: pattern found {count} times", flush=True)
            missed.append(m["name"] + " (pattern)")
            continue
        file.write_text(original.replace(m["old"], m["new"]))
        result = subprocess.run(
            ["cargo", "test", "--lib", "--", *filters],
            cwd="/home/user/venue-ports", capture_output=True, text=True)
        out = result.stdout + result.stderr
        if "error[" in out or "error:" in out and "could not compile" in out:
            print(f"BUILD-ERROR {m['name']}", flush=True)
            missed.append(m["name"] + " (does not build)")
        elif result.returncode != 0:
            failed = [l.split(" ... ")[0].replace("test ", "") for l in out.splitlines() if l.endswith("FAILED")]
            print(f"caught  {m['name']}: {len(failed)} failed, e.g. {failed[:1]}", flush=True)
        else:
            print(f"MISSED  {m['name']}", flush=True)
            missed.append(m["name"])
finally:
    shutil.copy(backup, file)
    backup.unlink()
print("missed:", missed if missed else "none")
