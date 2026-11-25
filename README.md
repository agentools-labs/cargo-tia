# cargo-tia

Test impact analysis for Cargo workspaces. `cargo tia` finds the workspace packages that a change can
affect and tests only those packages. When it cannot be sure, it tests the full workspace.

It is a single binary with one dependency (`serde_json`). It reads the local git repository and
`cargo metadata`. It makes no network calls and sends no telemetry.

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
cargo tia args                     # print `-p a -p b`, or `--workspace`
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
$ cargo tia --committed --base 4a51093^1
base: 4a51093^1 (merge base 669cb4c755db)
changed files: 1
  pkg/brew/ripgrep-bin.rb  full run: outside every package
selected: full workspace, 10 packages
  because `pkg/brew/ripgrep-bin.rb` is outside every package
```

With an empty selection, `test` and `nextest` run nothing and exit 0. `args` prints an empty line, so
`cargo test $(cargo tia args)` then tests the default members. Use `cargo tia test` when you want to skip
the run.

## How it selects

1. It finds the merge base of the base revision and `HEAD`. The default base is `origin/HEAD`, else
   `origin/main`, else `origin/master`.
2. It lists the files that changed since the merge base. The list includes staged, unstaged and
   untracked files. With `--committed`, it includes only committed changes.
3. It reads the workspace with `cargo metadata --no-deps --offline`.
4. It gives each changed file one effect, in this order:
   - **Full run**: the file matches a built-in or configured full-run glob, or it is a build script.
   - **Ignored**: the file matches an ignore glob.
   - **Package**: the file is in the directory of a workspace member. The deepest member wins.
   - **Path dependency**: the file is in a path dependency that is not a member. The members that use
     it are changed.
   - **Outside**: the file is in no package. This selects the full workspace.
5. It selects the changed packages and every member that depends on them, directly or through other
   members. Normal, dev, build, optional and target-specific dependencies all count.

These changes select the full workspace:

- `Cargo.toml` at the workspace root, and `Cargo.lock`.
- `rust-toolchain`, `rust-toolchain.toml`, `.cargo/config` and `.cargo/config.toml`, at any depth.
- `build.rs` at any depth, and each build script that `cargo metadata` reports.
- A file outside every package, for example a root `README.md`, `.github/` or `scripts/`. A package at
  the workspace root owns only the top-level entries that hold its targets, such as `src/` and
  `tests/`.
- A file that matches a configured `full-run` glob.
- Any case where git cannot give the change set: no merge base (for example in a shallow clone), an
  unknown base, or no repository.

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
          fetch-depth: 0  # the merge base must be in the clone
      - run: cargo install --locked --git https://github.com/agentools-labs/cargo-tia
      - run: cargo tia test --base "origin/$BASE_REF"
        env:
          BASE_REF: ${{ github.base_ref }}
```

This repository uses the tool on itself in [its CI workflow](.github/workflows/ci.yml).

## Limits

- The unit of selection is a package. A selected package runs all of its tests.
- The tool sees the inputs that Cargo declares. It does not see files that a package reads from another
  package's directory (`include_str!("../../other/x")`, shared fixtures), environment variables,
  services or external tools. Add shared inputs as `full-run` globs.
- `cargo test -p a` unifies features across the selected packages only. A build can differ from the
  `--workspace` build.
- Any `Cargo.lock` change selects the full workspace. The tool does not compare the lockfile per
  package.
- `-p NAME` is ambiguous when a member and a registry dependency have the same name. Cargo then reports
  an error.
- Windows is not tested.

## Measurements

MEASUREMENTS

## Alternatives

ALTERNATIVES

## Planned

- A per-test tier: map tests to the source lines they run, from a periodic full run with coverage, and
  fall back to the package tier when the map is stale or missing. It is left out because it would more
  than double the code and needs instrumented builds.
- Compare `Cargo.lock` per package, so that a dependency update selects only the packages that use the
  changed dependency.
- Publish on crates.io.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option.
