#!/usr/bin/env python3
"""Replays recent commits of a Cargo workspace and compares cargo-tia with full runs.

Usage: measure.py CARGO_TIA REPO_DIR SELECT_COMMITS TIMED_COMMITS OUT_JSON

For each of the last SELECT_COMMITS first-parent commits C, it records the selection of
`cargo tia list --json --committed --base C^1` at C. For the newest TIMED_COMMITS of them,
it builds the workspace tests, then times `cargo test --workspace --no-fail-fast` and
`cargo test <selection> --no-fail-fast`, and records the packages with failing tests in
each run. A package that fails in the full run but is not selected is a missed failure. A
full run that fails to compile, or exits non-zero without naming a failing package, makes
the commit inconclusive.
Set CARGO_TARGET_DIR to keep build output out of the shared target directory.
"""

import json
import re
import subprocess
import sys
import time


def run(args, cwd, check=True):
    result = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise SystemExit(f"{args} failed:\n{result.stderr}")
    return result


def failing_packages(stderr):
    return sorted(set(re.findall(r"`-p (\S+?)(?:@\S+)? ", stderr)))


def compile_failures(stderr):
    return sorted(set(re.findall(r"could not compile `([^`]+)`", stderr)))


def cargo_test(args, cwd):
    start = time.monotonic()
    result = run(["cargo", "test", *args, "--no-fail-fast"], cwd, check=False)
    seconds = time.monotonic() - start
    failing = failing_packages(result.stderr)
    broken = compile_failures(result.stderr)
    return {
        "seconds": round(seconds, 1),
        "exit": result.returncode,
        "compile_failures": broken,
        "failing_packages": failing,
        "inconclusive": result.returncode != 0 and (bool(broken) or not failing),
    }


def main():
    tia, repo, select_count, timed_count, out = sys.argv[1:6]
    select_count, timed_count = int(select_count), int(timed_count)
    start_ref = run(["git", "rev-parse", "HEAD"], repo).stdout.strip()
    commits = run(
        ["git", "rev-list", "--first-parent", "-n", str(select_count), start_ref], repo
    ).stdout.split()
    rows = []
    try:
        for index, commit in enumerate(commits):
            run(["git", "checkout", "-q", "--detach", commit], repo)
            started = time.monotonic()
            listed = run(
                [tia, "list", "--json", "--committed", "--base", f"{commit}^1"], repo
            )
            selection_seconds = time.monotonic() - started
            data = json.loads(listed.stdout)
            row = {
                "commit": commit,
                "subject": run(["git", "log", "-1", "--format=%s", commit], repo).stdout.strip(),
                "changed_files": len(data["changed_files"]),
                "workspace_packages": data["workspace_packages"],
                "full": data["full"],
                "full_reasons": data["full_reasons"],
                "selected": [p["name"] for p in data["selected"]],
                "selection_seconds": round(selection_seconds, 2),
            }
            if index < timed_count:
                build = run(["cargo", "test", "--workspace", "--no-run"], repo, check=False)
                row["build_ok"] = build.returncode == 0
                row["full_run"] = cargo_test(["--workspace"], repo)
                if data["selected"]:
                    row["selected_run"] = cargo_test(data["cargo_args"], repo)
                else:
                    row["selected_run"] = {"seconds": 0.0, "exit": 0, "compile_failures": [],
                                           "failing_packages": [], "inconclusive": False}
                selected = set(row["selected"])
                full = row["full_run"]
                row["missed_failures"] = [
                    p for p in full["failing_packages"] + full["compile_failures"]
                    if p not in selected
                ]
            rows.append(row)
            print(json.dumps(row), flush=True)
    finally:
        run(["git", "checkout", "-q", "--detach", start_ref], repo, check=False)
        with open(out, "w") as handle:
            json.dump(rows, handle, indent=2)


if __name__ == "__main__":
    main()
