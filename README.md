# cargo-tia

Test impact analysis for Cargo workspaces. `cargo tia` finds the workspace packages that a change can
affect and tests only those packages. When it cannot be sure, it tests the full workspace.

It is a single binary with one dependency (`serde_json`). It reads the local git repository and
`cargo metadata`. It makes no network calls and sends no telemetry. (`cargo tia test` then runs a
normal `cargo test`, which can download dependencies like any Cargo build.)

## Install

```sh
cargo install --locked --git https://github.com/agentools-labs/cargo-tia
```

`git` and `cargo` must be on `PATH`. Linux and macOS are supported.

## Use

```sh
cargo tia                          # show the selection against the default branch of origin
cargo tia --base origin/release    # compare with another branch
cargo tia list --json              # the selection as JSON
cargo tia args                     # print the cargo arguments for the selection
cargo tia test -- --no-fail-fast   # run `cargo test` on the selection
cargo tia nextest -- --profile ci  # run `cargo nextest run` on the selection
```

Example, on a commit of [ripgrep](https://github.com/BurntSushi/ripgrep/commit/3e85c6c) (10 packages):

```text
$ cargo tia --committed --base 3e85c6c^1
base: 3e85c6c^1 (merge base 4a51093f78bf)
changed files: 2
  crates/printer/src/standard.rs  grep-printer
  tests/feature.rs                ripgrep
selected: 3 of 10 packages
  grep          depends on grep-printer
  grep-printer  changed
  ripgrep       changed
```

A commit that changes a file outside every package selects the full workspace:

```text
$ cargo tia --committed --base 0856f39^1
base: 0856f39^1 (merge base 9c49a2589445)
changed files: 1
  .github/workflows/release.yml  full run: outside every package
selected: full workspace, 10 packages
  because `.github/workflows/release.yml` is outside every package
```

To skip such files, add an ignore glob, for example `--ignore '.github/**'` (see [Configure](#configure)).

The cargo arguments are `--workspace` for the full workspace. For a partial selection they are
`--workspace` plus `--exclude NAME` for each package that is not selected. They start with
`--manifest-path PATH` when you give one. `-p NAME` is not used: it is
ambiguous when a dependency has the same name and version as a member, which happens in real workspaces
(tracing is one). `--exclude` matches workspace members only.

With an empty selection, `test` and `nextest` run nothing and exit 0. `args` prints an empty line, so
`cargo test $(cargo tia args)` then tests the default members. Use `cargo tia test` when you want to skip
the run. `args` separates arguments with spaces, so for a manifest path that holds spaces, read the
`cargo_args` array of `cargo tia list --json` instead.

Arguments after `--` go to the test command. They must not change the packages: the tool rejects
`-p` and `-m` (also in a group such as `-qp`), `--package`, `--workspace`, `--exclude` and
`--manifest-path` there. A configuration option there (Cargo's `--config`, nextest's `--config-file` or
`--tool-config-file`) can change every package, so it selects the full workspace.

## How it selects

1. It finds the merge base of the base revision and `HEAD`. The default base is `origin/HEAD`. When
   `origin/HEAD` is not set, the tool does not guess a branch: pass `--base`.
2. It lists the files that changed since the merge base. The list includes staged, unstaged and
   untracked files, and edits to files marked assume-unchanged or skip-worktree. Without a sparse
   checkout, a missing skip-worktree file counts as deleted. Git configuration
   such as `diff.ignoreSubmodules`, `core.fsmonitor` or `core.fileMode` cannot hide a change. With `--committed`, the list includes only
   committed changes.
3. It reads the workspace with `cargo metadata --no-deps --offline`.
4. It gives each changed file one effect, in this order:
   - **Full run**: the file matches a built-in or configured full-run glob, or it is a build script.
   - **Ignored**: the file matches an ignore glob.
   - **Package**: the file is in the directory of a workspace member. The deepest member wins.
   - **Path dependency**: the file is in a path dependency that is not a member, or in a path
     dependency of such a dependency. The tool reads those manifests as text to find them, with the
     dependencies that they inherit (`workspace = true`) from their own workspace manifest. The
     members that use the first one are changed.
   - **Outside**: the file is in no package. This selects the full workspace.

   A changed file that a tracked symbolic link points to also changes the package that holds the link.
   The report shows that path with `(link to ...)`. A link to the workspace root or above it reaches
   every file, so any change changes the package that holds it.
5. It selects the changed packages and every member that depends on them, directly or through other
   members. Normal, dev, build, optional and target-specific dependencies all count. Cargo does not
   report every edge, so two kinds of members count as dependents of every member:
   - a member that uses a path dependency that is not a member;
   - a member with registry or git dependencies, when a `[patch]` or `[replace]` entry points to a
     member: those dependencies can depend on the patched member. This counts for the patched
     members only. A local patch path that is not a member counts for every member, and it is a
     path dependency of each member with registry or git dependencies. Cargo `paths` overrides
     count the same way. The tool reads patches and overrides as text from the workspace manifest
     and from the Cargo configuration files in the workspace root, the current directory, their
     parents and `CARGO_HOME`, and from the files that those configuration files include.

These changes select the full workspace:

- `Cargo.toml` and `Cargo.lock` at any depth. A member manifest can change the features that Cargo
  unifies for other packages, so it counts as a workspace file.
- `rust-toolchain`, `rust-toolchain.toml`, `.cargo/config` and `.cargo/config.toml`, at any depth.
- `build.rs` at any depth, each build script that `cargo metadata` reports, and the custom build
  scripts of path dependencies that are not members.
- A changed symbolic link or submodule, or an untracked nested repository. Its effect on other paths
  is unknown.
- A Cargo configuration file that another configuration file includes.
- A file in a source replacement directory (`directory` or `local-registry` in Cargo configuration).
  Any member can use the registry packages there.
- A file in a directory that holds a target of another package, for example
  `[[test]] path = "../other/check.rs"`. This includes targets of the root package below a member.
- A file outside every package, for example a root `README.md`, `.github/` or `scripts/`. A package at
  the workspace root owns only the top-level entries that hold its targets, such as `src/` and
  `tests/`.
- A file that matches a configured `full-run` glob.
- Any case where git cannot give a single change set: a directory that git cannot read, no merge base
  (for example in a shallow clone),
  more than one merge base (criss-cross merges), an unknown base, no `origin/HEAD` and no `--base`, or no
  repository.
- A path dependency, local patch, `paths` override, included configuration file, target, build
  script, file that a non-member manifest names, or symbolic link target outside the git repository, also through a path dependency
  of a path dependency. Git cannot see changes there.
- A non-member path dependency that inherits dependencies from a workspace manifest that the tool
  cannot find.
- With `--committed`: a `Cargo.toml` with uncommitted changes, or one that git hides with
  assume-unchanged or skip-worktree. The tool reads the workspace from the working tree, so the
  working tree must match `HEAD` there.

A dependency on a registry package with the same name as a member also counts as an edge to that
member, because `[patch]` can redirect it. This can select too much, never too little.

## Configure

Add globs to the root `Cargo.toml`. The workspace needs a `[workspace]` table.

```toml
[workspace.metadata.tia]
full-run = ["proto/**", "ci/**"]   # these files select the full workspace
ignore = ["docs/**", "*.md"]       # these files select nothing
```

`--full-run GLOB` and `--ignore GLOB` add globs on the command line. Glob rules:

- A glob matches the whole path, relative to the workspace root.
- `*` and `?` match inside one path segment. `**` matches any number of segments.
- A trailing `/` matches everything below a directory: `docs/` is `docs/**`.
- Files outside the workspace root have `../` paths, for example `../web/**`.
- Full-run globs win over ignore globs. The built-in full-run files cannot be ignored.

## Use in CI

Test the selection on pull requests. Keep a full run on the default branch, for example after each merge
or on a schedule, because some test inputs are invisible to the tool (see [Limits](#limits)).

```yaml
on: pull_request
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 2  # the merge commit and its parents
      - run: cargo install --locked --git https://github.com/agentools-labs/cargo-tia
      - run: cargo tia test --base HEAD^1
```

For a pull request, `actions/checkout` checks out a merge commit. Its first parent, `HEAD^1`, is the
target branch at that time, so the selection covers exactly the changes of the pull request, also when
the workflow runs again later. `origin/HEAD` is not set in this checkout, so pass `--base`.

This repository uses the tool on itself in [its CI workflow](.github/workflows/ci.yml).

## Limits

- The unit of selection is a package. With `cargo tia test`, a selected package runs all of its tests.
  Nextest does not run doctests, so with `cargo tia nextest` also run `cargo tia test -- --doc`
  when the selection holds a library (Cargo rejects `--doc` for binaries only).
- The tool sees the inputs that Cargo declares and the directory that holds each file. It does not see
  files that a package reads from another package's directory (`include!`, `include_str!`,
  `#[path = "../../other/x.rs"]`, shared fixtures), environment variables, services or external tools.
  Add shared inputs as `full-run` globs.
- A partial selection builds with the features of the selected packages only. A build can differ from
  the `--workspace` build. Run the full workspace on the default branch to catch this.
- From Cargo configuration, the tool reads `[patch]`, `[replace]`, `paths`, `include` and source
  replacement directories. A change to any Cargo configuration file selects the full workspace. Other
  settings that never change in the compared commits, such as environment variables, are not read.
- Any `Cargo.lock` change selects the full workspace. The tool does not compare the lockfile per
  package.
- In a partial clone, git 2.45 and newer cannot fetch missing objects for the tool: the command fails
  and the tool selects the full workspace. Older git also gets no transport, through
  `GIT_ALLOW_PROTOCOL`.
- Run the tool as `cargo tia`: Cargo then gives it the path of its own binary. Run directly as
  `cargo-tia`, it calls `cargo` through rustup, and rustup older than 1.28 can download a missing
  toolchain that the workspace pins.
- To compare the working tree, git runs the clean filters that the repository configures, for example
  Git LFS, as `git status` does. The tool turns off `core.fsmonitor` hooks.
- Windows is not tested.

## Measurements

Measured on 2026-11-25 on an Apple Silicon Mac with Rust 1.99.1, with
[scripts/measure.py](scripts/measure.py) and the default configuration (no ignore globs). Each commit
`C` is compared with its first parent: `cargo tia list --json --committed --base C^1`.

**Selection** over the last 50 first-parent commits of each workspace. One selection takes 0.1 to
0.3 seconds.

| Workspace | Packages | Partial | Empty | Full | Mean selected |
|---|---|---|---|---|---|
| [ripgrep](https://github.com/BurntSushi/ripgrep) at `3e85c6c` | 10 | 28 | 2 | 20 | 5.5 (55%) |
| [tracing](https://github.com/tokio-rs/tracing) at `8d4d4d6` | 16 | 28 | 0 | 22 | 13.2 (82%) |
| [rust-analyzer](https://github.com/rust-lang/rust-analyzer) at `9c74e78e0` | 46 | 37 | 0 | 13 | 21.2 (46%) |

Most full selections come from files outside every package (CI workflows, documents, release files)
and from manifest changes. In tracing, 19 of the 22 full selections come from member `Cargo.toml`
changes. Ignore globs for documents and CI files change the result:

- ripgrep with `*.md`, `.github/**`, `pkg/**`, `HomebrewFormula/**` and `cliff.toml` ignored: 6 full,
  32 partial, 12 empty, mean 2.9 packages (29%).
- tracing with `*.md` and `.github/**` ignored: mean 12.5 packages (78%).
- rust-analyzer with `*.md`, `.github/**`, `editors/**` and `docs/**` ignored: 6 full, 38 partial, 6
  empty, mean 14.9 packages (33%).

**Test time** for the newest commits. For each commit, the script builds the workspace and runs its
tests once without timing, then times `cargo test --workspace` and `cargo test` with the selection.
So the numbers compare test time with a warm build. They do not include the compile time that a
selection saves in a cold build. A selection with other features can compile some packages again,
and that time counts against the selection.

| Workspace | Commits (partial) | Full runs | Selected runs | Saved | Saved on partial commits |
|---|---|---|---|---|---|
| ripgrep | 10 (3) | 46.5 s | 37.5 s | 19% | 65% |
| tracing | 8 (6) | 175.7 s | 157.3 s | 10% | 14% |
| rust-analyzer | 5 (3) | 94.4 s | 91.0 s | 4% | 6% |

In tracing and rust-analyzer, most changes reach the central packages, whose tests take most of the
time. One rust-analyzer selection (14 of 46 packages) took 20.6 s against 14.8 s for the full run,
because it compiled packages again with other features.

**Missed failures.** No test failed in the 23 full runs, so the history replay could not miss one. A
fault injection on ripgrep at `3e85c6c` and `b674410` broke code and kept the tests. Every package
that failed in the full run was in the selection:

| Fault | Selected | Failing packages in the full run | Missed |
|---|---|---|---|
| Revert the fix in `crates/printer/src/standard.rs` | 3 of 10 | ripgrep | none |
| Revert `crates/core/flags/{defs,hiargs}.rs` | 1 of 10 | ripgrep | none |
| Invert `GlobSet::is_match` | 5 of 10 | globset, grep-cli, ripgrep | none |
| Invert the default `Matcher::is_match` | 7 of 10 | grep-pcre2, grep-printer, grep-regex, grep-searcher, ripgrep | none |

## Alternatives

Checked on 2026-11-25 on crates.io, GitHub and docs.rs. A search of r/rust found no firsthand reports on
these tools. The closest firsthand discussion is a Rust internals thread,
[Cargo idea: only rerun tests in crates with changes](https://internals.rust-lang.org/t/cargo-idea-cli-option-to-only-rerun-tests-in-crates-with-changes/22108).
The CircleCI article
[Run fewer tests](https://circleci.com/blog/run-fewer-tests-speed-up-ci-smarter-testing/) (2026-10-22)
describes the same idea for other languages: per-test coverage maps, plus "full test run paths" that
always run every test. It names no Rust support.

Package-level selection, like this tool:

- [cargo-delta](https://github.com/tekian/cargo-delta) (MIT, 0.5.0, 2026-10, about 94,000 downloads).
  Maintained, stable Rust. It parses Rust modules, `include_str!` and file-loading calls to map files
  to packages, and it narrows `Cargo.lock` and root manifest changes. A changed file that its parser
  does not map selects nothing, and its trip-wire list is empty by default. Its README asks for a
  separate backstop build.
- [haf-cargo-affected](https://github.com/huuff/cargo-affected) (WTFPL, 0.1.0, 2026-11). The design
  closest to this tool: merge-base default, workspace files that select everything, all dependency
  kinds, lockfile narrowing and JSON output. A file outside every crate that no rule matches is
  ignored. It does not run tests. Its first release was two weeks old when checked.
- [cargo-rail](https://github.com/loadingalias/cargo-rail) (MIT, 0.32.0, 2026-10, 232 stars).
  Maintained. A large workspace engine (about 240,000 lines) with affected-work planning, a remote
  compiler cache, releases and repository split. Conservative, but not simple.
- [cargo-test-changed](https://github.com/felixpackard/cargo-test-changed) (MIT or Apache-2.0, 0.1.1,
  2025-04). `--with-dependents` adds direct dependents only, not transitive ones, and workspace files
  do not select everything. The last code change was in 2025-08.
- [determinator](https://github.com/guppy-rs/guppy/tree/main/tools/determinator) (MIT or Apache-2.0,
  0.13.0, 2026-10, part of guppy). Fail-safe rules: an unmatched path selects everything. It is a
  library with no command-line tool, and it needs `cargo metadata` of both revisions.
- [cargo-nextest](https://nexte.st/) filtersets such as `rdeps(name)` select the reverse dependencies
  of given packages, and `--partition` splits a run across machines. Nextest does not find changed
  packages.
- [cargo-workspaces](https://github.com/pksunkara/cargo-workspaces) `changed` lists packages changed
  since the last release tag, for versioning. It does not add dependents for testing.

Per-test selection from coverage:

- [cargo-difftests](https://github.com/dnbln/cargo-difftests) (Apache-2.0, 0.6.1, 2024-02). Needs
  nightly Rust. No changes since 2024-05.
- [cargo-affected](https://github.com/max-sixty/cargo-affected) (MIT or Apache-2.0, 0.4.1, 2026-09).
  Per-test LLVM coverage on stable Rust with `llvm-tools` and nextest. Falls back to all tests when its
  coverage data is stale. Its author marks it "extremely early" and says that CI should still run the
  full suite.
- [testtrim](https://codeberg.org/testtrim/testtrim) (GPL-3.0, 0.14.11, 2026-09). Coverage-based for
  several languages, Rust included. It can share its database through PostgreSQL or a web service.
- [cargo-impact](https://github.com/asmuelle/cargo-impact) (MIT or Apache-2.0, 0.5.0, 2026-05).
  Delegates per-test mapping to cargo-difftests.

No maintained tool combined a small, self-contained command with fail-safe defaults: select everything
when a change is not understood. This tool fills that gap at the package level.

## Planned

- A per-test tier: map tests to the source lines they run, from a periodic full run with coverage, and
  fall back to the package tier when the map is stale or missing. It is left out because it would more
  than double the code and needs instrumented builds.
- Read the rustc dependency files (`target/*/deps/*.d`) of the last build to find the files that each
  package compiles from other packages' directories (`include!`, `#[path]`).
- Compare `Cargo.lock` per package, so that a dependency update selects only the packages that use the
  changed dependency.
- Publish on crates.io.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option.
