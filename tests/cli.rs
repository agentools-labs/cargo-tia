//! End-to-end tests: generated workspaces in temporary git repositories.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_cargo-tia");

/// A temporary git repository, removed on drop.
struct Repo {
    dir: PathBuf,
}

/// A member named `name` in `crates/<name>`, with dependencies as (name, table) pairs, where
/// the table is `dependencies`, `dev-dependencies` or `build-dependencies`.
type Member<'a> = (&'a str, &'a [(&'a str, &'a str)]);

impl Repo {
    fn new(name: &str) -> Repo {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join("cargo-tia-tests")
            .join(format!("{name}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repo { dir };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    /// A committed virtual workspace at the repository root.
    fn with_workspace(name: &str, members: &[Member]) -> Repo {
        let repo = Repo::new(name);
        repo.write_workspace("", members);
        repo.commit("initial");
        repo
    }

    /// Writes a virtual workspace in the directory `prefix` (empty for the repository root).
    fn write_workspace(&self, prefix: &str, members: &[Member]) {
        let at = |path: String| {
            if prefix.is_empty() {
                path
            } else {
                format!("{prefix}/{path}")
            }
        };
        let list: Vec<String> = members
            .iter()
            .map(|(name, _)| format!("\"crates/{name}\""))
            .collect();
        self.write(
            &at("Cargo.toml".to_string()),
            &format!(
                "[workspace]\nresolver = \"2\"\nmembers = [{}]\n",
                list.join(", ")
            ),
        );
        for (name, deps) in members {
            let mut manifest =
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
            for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
                manifest += &format!("\n[{table}]\n");
                for (dep, _) in deps.iter().filter(|(_, kind)| *kind == table) {
                    manifest += &format!("{dep} = {{ path = \"../{dep}\" }}\n");
                }
            }
            self.write(&at(format!("crates/{name}/Cargo.toml")), &manifest);
            self.write(
                &at(format!("crates/{name}/src/lib.rs")),
                "pub fn f() -> u32 { 1 }\n",
            );
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn append(&self, relative: &str, contents: &str) {
        let old = fs::read_to_string(self.path(relative)).unwrap_or_default();
        self.write(relative, &(old + contents));
    }

    fn git(&self, args: &[&str]) -> String {
        let output = isolated(Command::new("git"))
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    fn tia_in(&self, dir: &Path, args: &[&str]) -> Output {
        isolated(Command::new(BIN))
            .current_dir(dir)
            .env("CARGO_TARGET_DIR", self.path("target"))
            .args(args)
            .output()
            .unwrap()
    }

    fn tia(&self, args: &[&str]) -> Output {
        self.tia_in(&self.dir, args)
    }

    /// `cargo tia list --json` against `base`.
    fn select(&self, base: &str, extra: &[&str]) -> Value {
        let mut args = vec!["tia", "list", "--json", "--base", base];
        args.extend(extra);
        let output = self.tia(&args);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Keeps the user's git configuration out of the tests.
fn isolated(mut command: Command) -> Command {
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    command
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn selected(json: &Value) -> Vec<String> {
    json["selected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

fn is_full(json: &Value) -> bool {
    json["full"].as_bool().unwrap()
}

/// core <- mid <- app, core <-(dev) tester, core <-(build) builder; lonely is independent.
fn chain() -> Repo {
    Repo::with_workspace(
        "chain",
        &[
            ("core", &[]),
            ("mid", &[("core", "dependencies")]),
            ("app", &[("mid", "dependencies")]),
            ("tester", &[("core", "dev-dependencies")]),
            ("builder", &[("core", "build-dependencies")]),
            ("lonely", &[]),
        ],
    )
}

#[test]
fn change_selects_package_and_all_dependent_kinds() {
    let repo = chain();
    let base = repo.git(&["rev-parse", "HEAD"]);
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("change core");
    let json = repo.select(base.trim(), &[]);
    assert!(!is_full(&json));
    assert_eq!(selected(&json), ["app", "builder", "core", "mid", "tester"]);
    assert_eq!(json["workspace_packages"], 6);
    let app = json["selected"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "app")
        .unwrap();
    assert_eq!(app["reason"], "dependent");
    assert_eq!(app["via"], "mid");
}

#[test]
fn leaf_change_selects_only_the_leaf() {
    let repo = chain();
    repo.append("crates/app/src/lib.rs", "pub fn g() {}\n");
    let json = repo.select("HEAD", &[]);
    assert_eq!(selected(&json), ["app"]);
    assert_eq!(json["cargo_args"], serde_json::json!(["-p", "app"]));
}

#[test]
fn no_change_selects_nothing() {
    let repo = chain();
    let json = repo.select("HEAD", &[]);
    assert!(!is_full(&json));
    assert!(selected(&json).is_empty());
    let output = repo.tia(&["args", "--base", "HEAD"]);
    assert_eq!(stdout(&output).trim(), "");
    let output = repo.tia(&["test", "--base", "HEAD"]);
    assert!(output.status.success());
    assert!(
        stderr(&output).contains("nothing to run"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn full_run_files_select_the_whole_workspace() {
    for path in [
        "Cargo.lock",
        "Cargo.toml",
        "rust-toolchain",
        "rust-toolchain.toml",
        ".cargo/config.toml",
        ".cargo/config",
        "crates/mid/.cargo/config.toml",
        "crates/mid/build.rs",
        "scripts/release.sh",
        "README.md",
    ] {
        let repo = chain();
        repo.append(path, "\n# changed\n");
        let json = repo.select("HEAD", &[]);
        assert!(is_full(&json), "{path}: {json:#}");
        assert_eq!(selected(&json).len(), 6, "{path}");
        assert_eq!(
            json["cargo_args"],
            serde_json::json!(["--workspace"]),
            "{path}"
        );
        assert!(!json["full_reasons"].as_array().unwrap().is_empty());
    }
}

#[test]
fn custom_build_script_paths_select_the_whole_workspace() {
    let repo = chain();
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "edition = \"2021\"\n",
            "edition = \"2021\"\nbuild = \"gen/main.rs\"\n",
        ),
    );
    repo.write("crates/lonely/gen/main.rs", "fn main() {}\n");
    repo.commit("custom build script");
    repo.append("crates/lonely/gen/main.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert_eq!(json["changed_files"][0]["detail"], "is a build script");
}

#[test]
fn configured_globs_force_or_ignore() {
    let repo = chain();
    repo.append(
        "Cargo.toml",
        "\n[workspace.metadata.tia]\nfull-run = [\"ci/**\"]\nignore = [\"docs/**\", \"**/*.md\"]\n",
    );
    repo.commit("configure");
    repo.write("docs/guide.txt", "guide\n");
    repo.write("crates/core/NOTES.md", "notes\n");
    let json = repo.select("HEAD", &[]);
    assert!(!is_full(&json), "{json:#}");
    assert!(selected(&json).is_empty());

    repo.write("ci/run.sh", "echo\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json));

    // Command-line globs add to the configured ones.
    let json = repo.select("HEAD", &["--ignore", "ci/**"]);
    assert!(is_full(&json), "full-run globs win over ignore globs");
    std::fs::remove_dir_all(repo.path("ci")).unwrap();
    repo.write("fixtures/data.json", "{}\n");
    assert!(is_full(&repo.select("HEAD", &[])));
    let json = repo.select("HEAD", &["--ignore", "fixtures/"]);
    assert!(!is_full(&json));
    repo.write("crates/lonely/data.json", "{}\n");
    let json = repo.select(
        "HEAD",
        &["--ignore", "fixtures/", "--full-run", "**/*.json"],
    );
    assert!(is_full(&json));
}

#[test]
fn working_tree_changes_count_unless_committed_only() {
    let repo = chain();
    repo.write("crates/lonely/src/new.rs", "// untracked\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
    assert!(selected(&repo.select("HEAD", &["--committed"])).is_empty());

    repo.git(&["add", "crates/lonely/src/new.rs"]);
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"], "staged");
    repo.commit("add new.rs");
    assert_eq!(
        selected(&repo.select("HEAD~1", &["--committed"])),
        ["lonely"]
    );
}

#[test]
fn deletions_and_renames_select_both_sides() {
    let repo = chain();
    repo.write("crates/lonely/src/extra.rs", "// extra\n");
    repo.commit("extra");
    repo.git(&[
        "mv",
        "crates/lonely/src/extra.rs",
        "crates/app/src/extra.rs",
    ]);
    repo.commit("move");
    assert_eq!(selected(&repo.select("HEAD~1", &[])), ["app", "lonely"]);

    repo.write("crates/mid/src/old.rs", "// old\n");
    repo.commit("old");
    fs::remove_file(repo.path("crates/mid/src/old.rs")).unwrap();
    assert_eq!(selected(&repo.select("HEAD", &[])), ["app", "mid"]);
}

#[test]
fn removed_package_selects_the_whole_workspace() {
    let repo = chain();
    // With a glob in `members`, removing a package leaves the root manifest unchanged.
    repo.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    repo.commit("glob members");
    fs::remove_dir_all(repo.path("crates/lonely")).unwrap();
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert_eq!(json["changed_files"][0]["effect"], "outside");
    assert_eq!(selected(&json).len(), 5);
}

#[test]
fn default_base_is_the_merge_base_with_origin() {
    let origin = chain();
    let clone_parent = Repo::new("clone");
    let clone_dir = clone_parent.path("work");
    let status = isolated(Command::new("git"))
        .args(["clone", "-q"])
        .arg(&origin.dir)
        .arg(&clone_dir)
        .status()
        .unwrap();
    assert!(status.success());
    // Removed on drop, as is its parent.
    let clone = Repo { dir: clone_dir };
    clone.git(&["checkout", "-q", "-b", "feature"]);
    clone.append("crates/mid/src/lib.rs", "pub fn g() {}\n");
    clone.commit("feature work");
    // The default branch moves on after the fork; its changes are not the feature's.
    origin.append("crates/lonely/src/lib.rs", "pub fn h() {}\n");
    origin.commit("main work");
    clone.git(&["fetch", "-q", "origin"]);

    let output = clone.tia(&["tia", "list", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["base"], "origin/main");
    assert_eq!(selected(&json), ["app", "mid"]);
}

#[test]
fn unusable_base_or_repository_selects_the_whole_workspace() {
    let repo = chain();
    let json = repo.select("no-such-branch", &[]);
    assert!(is_full(&json));
    assert_eq!(json["base"], "no-such-branch");
    assert!(json["merge_base"].is_null());

    // No origin: the default base cannot be found.
    let output = repo.tia(&["list", "--json"]);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(is_full(&json));
    assert!(json["full_reasons"][0].as_str().unwrap().contains("--base"));

    // Not a git repository.
    fs::remove_dir_all(repo.path(".git")).unwrap();
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json));
}

#[test]
fn workspace_below_the_git_root() {
    let repo = Repo::new("subdir");
    repo.write("web/app.js", "// js\n");
    repo.write_workspace("rust", &[("core", &[]), ("app", &[])]);
    repo.commit("initial");

    repo.append("rust/crates/core/src/lib.rs", "pub fn g() {}\n");
    let output = repo.tia_in(&repo.path("rust"), &["list", "--json", "--base", "HEAD"]);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected(&json), ["core"]);
    assert_eq!(json["changed_files"][0]["path"], "crates/core/src/lib.rs");

    repo.append("web/app.js", "// changed\n");
    let output = repo.tia_in(&repo.path("rust"), &["list", "--json", "--base", "HEAD"]);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        is_full(&json),
        "a file outside the workspace root is outside every package"
    );
    assert_eq!(json["changed_files"][0]["path"], "../web/app.js");
    assert_eq!(json["changed_files"][0]["effect"], "outside");
    let output = repo.tia_in(
        &repo.path("rust"),
        &["list", "--json", "--base", "HEAD", "--ignore", "../web/**"],
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected(&json), ["core"]);
}

#[test]
fn manifest_path_selects_the_workspace() {
    let repo = chain();
    repo.append("crates/app/src/lib.rs", "pub fn g() {}\n");
    let manifest = repo.path("Cargo.toml");
    let output = repo.tia_in(
        &std::env::temp_dir(),
        &[
            "list",
            "--json",
            "--base",
            "HEAD",
            "--manifest-path",
            manifest.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected(&json), ["app"]);
}

#[test]
fn non_member_path_dependency_selects_its_users() {
    let repo = chain();
    repo.write(
        "vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("vendor/shim/src/lib.rs", "pub fn s() {}\n");
    repo.append("Cargo.toml", "exclude = [\"vendor/shim\"]\n");
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "[dependencies]\n",
            "[dependencies]\nshim = { path = \"../../vendor/shim\" }\n",
        ),
    );
    repo.commit("vendor shim");
    repo.append("vendor/shim/src/lib.rs", "pub fn t() {}\n");
    let json = repo.select("HEAD", &[]);
    assert!(!is_full(&json), "{json:#}");
    assert_eq!(selected(&json), ["lonely"]);
    assert_eq!(json["changed_files"][0]["effect"], "path-dependency");
}

#[test]
fn root_package_owns_only_its_targets() {
    let repo = Repo::new("rootpkg");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"rootpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nhelper = { path = \"crates/helper\" }\n\n[workspace]\nmembers = [\"crates/helper\"]\n",
    );
    repo.write("src/main.rs", "fn main() {}\n");
    repo.write("tests/it.rs", "#[test] fn t() {}\n");
    repo.write("README.md", "readme\n");
    repo.write(
        "crates/helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("crates/helper/src/lib.rs", "pub fn h() {}\n");
    repo.commit("initial");

    repo.append("tests/it.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["rootpkg"]);
    repo.git(&["checkout", "-q", "--", "."]);

    repo.append("crates/helper/src/lib.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["helper", "rootpkg"]);
    repo.git(&["checkout", "-q", "--", "."]);

    repo.append("README.md", "changed\n");
    assert!(is_full(&repo.select("HEAD", &[])));
}

#[test]
fn args_prints_package_flags() {
    let repo = chain();
    repo.append("crates/mid/src/lib.rs", "pub fn g() {}\n");
    let output = repo.tia(&["tia", "args", "--base", "HEAD"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).trim(), "-p app -p mid");
    repo.append("Cargo.lock", "\n");
    let output = repo.tia(&["args", "--base", "HEAD"]);
    assert_eq!(stdout(&output).trim(), "--workspace");
}

#[test]
fn human_report_explains_the_selection() {
    let repo = chain();
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    let output = repo.tia(&["--base", "HEAD"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("selected: 5 of 6 packages"), "{text}");
    assert!(text.contains("depends on mid"), "{text}");
    repo.append("Cargo.lock", "\n");
    let text = stdout(&repo.tia(&["--base", "HEAD"]));
    assert!(
        text.contains("selected: full workspace, 6 packages"),
        "{text}"
    );
    assert!(text.contains("because `Cargo.lock`"), "{text}");
}

#[test]
fn errors_exit_with_code_2() {
    let repo = chain();
    let output = repo.tia(&["--bogus"]);
    assert_eq!(output.status.code(), Some(2));
    repo.write("Cargo.toml", "this is not toml [");
    let output = repo.tia(&["--base", "HEAD"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("cargo metadata"),
        "{}",
        stderr(&output)
    );
}

/// Replaces `cargo` with a script that forwards `metadata` and records every other call.
#[cfg(unix)]
fn fake_cargo(repo: &Repo) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = repo.path("fake-cargo.sh");
    let log = repo.path("cargo-calls.log");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then exec '{}' \"$@\"; fi\necho \"$@\" >> '{}'\n",
            env!("CARGO"),
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn test_and_nextest_run_the_selection() {
    let repo = chain();
    fs::write(
        repo.path(".git/info/exclude"),
        "fake-cargo.sh\ncargo-calls.log\n",
    )
    .unwrap();
    let cargo = fake_cargo(&repo);
    repo.append("crates/mid/src/lib.rs", "pub fn g() {}\n");
    let run = |args: &[&str]| {
        let output = isolated(Command::new(BIN))
            .current_dir(&repo.dir)
            .env("CARGO", &cargo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    };
    run(&["tia", "test", "--base", "HEAD", "--", "--no-fail-fast"]);
    run(&["tia", "nextest", "--base", "HEAD", "--", "--profile", "ci"]);
    repo.append("Cargo.lock", "\n");
    run(&["tia", "nextest", "--base", "HEAD"]);
    let calls = fs::read_to_string(repo.path("cargo-calls.log")).unwrap();
    assert_eq!(
        calls,
        "test -p app -p mid --no-fail-fast\n\
         nextest run -p app -p mid --profile ci\n\
         nextest run --workspace\n"
    );
}

#[test]
fn test_runs_cargo_test_only_on_the_selection() {
    let repo = chain();
    repo.append(
        "crates/core/src/lib.rs",
        "#[test]\nfn core_works() { assert_eq!(f(), 1); }\n",
    );
    repo.append(
        "crates/lonely/src/lib.rs",
        "#[test]\nfn lonely_fails() { panic!(\"lonely is broken\"); }\n",
    );
    repo.commit("tests");

    repo.append("crates/mid/src/lib.rs", "// changed\n");
    let output = repo.tia(&["tia", "test", "--base", "HEAD"]);
    assert!(output.status.success(), "{}", stderr(&output));

    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    let output = repo.tia(&["tia", "test", "--base", "HEAD"]);
    assert_eq!(output.status.code(), Some(101), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("lonely is broken")
            || stderr(&output).contains("lonely is broken")
    );
}

/// A small deterministic generator, so the property test needs no extra crate.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Property: on generated workspaces with random dependency graphs, the selection contains
/// every changed package and never misses a dependent of a selected package.
#[test]
fn property_generated_workspaces_never_miss_a_dependent() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let tables = ["dependencies", "dev-dependencies", "build-dependencies"];
    for case in 0..8 {
        let count = 3 + rng.below(6);
        let names: Vec<String> = (0..count).map(|i| format!("p{i}")).collect();
        // Package i may depend on any j < i (normal, dev or build), and dev-depend on
        // any j > i: dev-dependency cycles are legal.
        let mut edges: Vec<Vec<(usize, &str)>> = vec![Vec::new(); count];
        for (i, deps) in edges.iter_mut().enumerate() {
            for j in 0..count {
                if j < i && rng.below(3) == 0 {
                    deps.push((j, tables[rng.below(3)]));
                } else if j > i && rng.below(8) == 0 {
                    deps.push((j, "dev-dependencies"));
                }
            }
        }
        let dep_lists: Vec<Vec<(&str, &str)>> = edges
            .iter()
            .map(|deps| deps.iter().map(|&(j, t)| (names[j].as_str(), t)).collect())
            .collect();
        let members: Vec<(&str, &[(&str, &str)])> = names
            .iter()
            .zip(&dep_lists)
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        let repo = Repo::with_workspace(&format!("prop{case}"), &members);

        for _ in 0..3 {
            let changed: Vec<usize> = (0..1 + rng.below(2)).map(|_| rng.below(count)).collect();
            for &i in &changed {
                repo.append(&format!("crates/p{i}/src/lib.rs"), "// change\n");
            }
            let json = repo.select("HEAD", &[]);
            assert!(!is_full(&json), "case {case}");
            let got = selected(&json);
            let has = |i: usize| got.contains(&names[i]);
            for &i in &changed {
                assert!(has(i), "case {case}: changed p{i} missing from {got:?}");
            }
            for (i, deps) in edges.iter().enumerate() {
                for &(j, _) in deps {
                    assert!(
                        !has(j) || has(i),
                        "case {case}: p{i} depends on p{j}; got {got:?}"
                    );
                }
            }
            repo.git(&["checkout", "-q", "--", "."]);
        }
    }
}
