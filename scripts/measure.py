#!/usr/bin/env python3
"""Replays recent commits of a Cargo workspace and compares cargo-tia with full runs.

Usage: measure.py CARGO_TIA REPO_DIR SELECT_COMMITS TIMED_COMMITS OUT_JSON

For each of the last SELECT_COMMITS first-parent commits C, it records the selection of
`cargo tia list --json --committed --base C^1` at C. For the newest TIMED_COMMITS of them,
it builds the workspace and runs its tests once untimed (the first run after a build does
extra work), then times `cargo test --workspace --no-fail-fast` and
`cargo test <selection> --no-fail-fast`, and records the packages with failing tests in
each run. Only the full run is warmed up: when the selection builds with other features, the
selected run pays that compilation, as it would in CI with a cache from full builds. A
package that fails in the full run but is not selected is a missed failure. A
full run that fails to compile, or exits non-zero without naming a failing package, makes
the commit inconclusive.
Run it in a clean, disposable clone: it checks out each commit (detached) and then returns to
the starting branch or commit. Set CARGO_TARGET_DIR to keep build output out of the shared
target directory.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import time


def run(args, cwd, check=True):
    result = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise SystemExit(f"{args} failed:\n{result.stderr}")
    return result


def failing_packages(stderr):
    named = set(re.findall(r"`-p (\S+?)(?:@\S+)? ", stderr))
    # In a workspace with one package, Cargo's hints name no package: "*" stands for it.
    if not named and re.search(r"to rerun pass `--", stderr):
        named.add("*")
    return sorted(named)


def compile_failures(stderr):
    return sorted(set(re.findall(r"could not compile `([^`]+)`", stderr)))


def cargo_test(args, cwd):
    start = time.monotonic()
    result = run(["cargo", "test", *args, "--no-fail-fast"], cwd, check=False)
    seconds = time.monotonic() - start
    # A run that exits 0 has no failures, whatever its warnings say.
    failing = failing_packages(result.stderr) if result.returncode != 0 else []
    broken = compile_failures(result.stderr)
    return {
        "seconds": round(seconds, 1),
        "exit": result.returncode,
        "compile_failures": broken,
        "failing_packages": failing,
        # A test runner that could not start names packages too, but ran no test.
        "inconclusive": result.returncode != 0 and (
            bool(broken) or not failing or "could not execute process" in result.stderr),
    }


def main():
    tia, repo, select_count, timed_count, out = sys.argv[1:6]
    # Subprocesses run inside the measured repository, so make the given paths absolute.
    tia = os.path.abspath(tia) if os.sep in tia else (shutil.which(tia) or tia)
    repo, out = os.path.abspath(repo), os.path.abspath(out)
    # The clone is cleaned between commits: the tool must live elsewhere.
    if os.path.abspath(tia).startswith(repo + os.sep):
        raise SystemExit("build or install cargo-tia outside the measured clone")
    # The clone is cleaned between runs: the warm build must live outside it.
    target_dir = os.environ.get("CARGO_TARGET_DIR", "")
    if not target_dir or os.path.abspath(target_dir).startswith(repo + os.sep):
        raise SystemExit("set CARGO_TARGET_DIR to a directory outside the measured clone")
    # Cargo runs inside the clone: a relative path would point into it.
    os.environ["CARGO_TARGET_DIR"] = os.path.abspath(target_dir)
    select_count, timed_count = int(select_count), int(timed_count)
    if run(["git", "status", "--porcelain", "--untracked-files=all"], repo).stdout.strip():
        raise SystemExit(f"{repo} has uncommitted changes; use a clean clone")
    # Checking out old commits would leave submodules at other revisions.
    if any(line.startswith("160000 ") for line in run(["git", "ls-files", "-s"], repo).stdout.splitlines()):
        raise SystemExit(f"{repo} has submodules, which this script does not replay")
    start_ref = run(["git", "rev-parse", "HEAD"], repo).stdout.strip()
    branch = run(["git", "symbolic-ref", "-q", "--short", "HEAD"], repo, check=False).stdout.strip()
    commits = run(
        ["git", "rev-list", "--first-parent", "-n", str(select_count), start_ref], repo
    ).stdout.split()
    # A commit without a parent (the root, or the end of a shallow clone) has nothing to compare.
    commits = [
        commit for commit in commits
        if run(["git", "rev-parse", "--verify", "-q", f"{commit}^1"], repo, check=False).returncode == 0
    ]
    print(f"measuring {len(commits)} commits", file=sys.stderr)
    rows = []
    try:
        for index, commit in enumerate(commits):
            # A build or a test can rewrite tracked files or leave files behind. The clone is
            # disposable and the build output lives outside it.
            run(["git", "reset", "-q", "--hard"], repo)
            run(["git", "clean", "-fdxq"], repo)
            run(["git", "checkout", "-q", "--detach", commit], repo)
            listing = run(["git", "ls-files", "-s"], repo).stdout.splitlines()
            if any(line.startswith("160000 ") for line in listing):
                row = {"commit": commit, "error": "submodules at this commit are not replayed"}
                rows.append(row)
                print(json.dumps(row), flush=True)
                continue
            started = time.monotonic()
            listed = run(
                [tia, "list", "--json", "--committed", "--base", f"{commit}^1"], repo,
                check=False,
            )
            selection_seconds = time.monotonic() - started
            if listed.returncode != 0:
                # For example, a manifest that does not parse at this commit.
                row = {"commit": commit, "error": listed.stderr.strip()}
                rows.append(row)
                print(json.dumps(row), flush=True)
                continue
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
                # Tests can write files: reset the clone before each run.
                clean = lambda: (run(["git", "reset", "-q", "--hard"], repo),
                                 run(["git", "clean", "-fdxq"], repo))
                clean()
                cargo_test(["--workspace"], repo)  # warm-up, untimed
                clean()
                row["full_run"] = cargo_test(["--workspace"], repo)
                if data["selected"]:
                    clean()
                    row["selected_run"] = cargo_test(data["cargo_args"], repo)
                else:
                    row["selected_run"] = {"seconds": 0.0, "exit": 0, "compile_failures": [],
                                           "failing_packages": [], "inconclusive": False}
                selected = set(row["selected"])
                full = row["full_run"]
                # Unknown when the full run is inconclusive: a compile failure can belong to a
                # dependency that no selection contains.
                row["missed_failures"] = None if full["inconclusive"] else [
                    p for p in full["failing_packages"]
                    if p not in selected and not (p == "*" and selected)
                ]
            rows.append(row)
            print(json.dumps(row), flush=True)
    finally:
        run(["git", "reset", "-q", "--hard"], repo, check=False)
        run(["git", "clean", "-fdxq"], repo, check=False)
        # Restore first: the report can be a file inside the clone.
        restore = ["git", "checkout", "-q", branch] if branch else [
            "git", "checkout", "-q", "--detach", start_ref]
        restored = run(restore, repo, check=False)
        with open(out, "w") as handle:
            json.dump(rows, handle, indent=2)
        if restored.returncode != 0:
            raise SystemExit(f"results saved, but cannot restore {repo}:\n{restored.stderr}")


if __name__ == "__main__":
    main()
