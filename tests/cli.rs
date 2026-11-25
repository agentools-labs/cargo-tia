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

/// Keeps the user's git and Cargo configuration out of the tests.
fn isolated(mut command: Command) -> Command {
    let cargo_home = std::env::temp_dir()
        .join("cargo-tia-tests")
        .join("cargo-home");
    let _ = fs::create_dir_all(&cargo_home);
    command
        .env("CARGO_HOME", cargo_home)
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
    assert_eq!(
        json["cargo_args"],
        serde_json::json!([
            "--workspace",
            "--exclude",
            "builder",
            "--exclude",
            "core",
            "--exclude",
            "lonely",
            "--exclude",
            "mid",
            "--exclude",
            "tester"
        ])
    );
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
        "crates/mid/Cargo.toml",
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
fn deleted_build_script_selects_the_whole_workspace() {
    let repo = chain();
    repo.write("crates/lonely/build.rs", "fn main() {}\n");
    repo.commit("build script");
    fs::remove_file(repo.path("crates/lonely/build.rs")).unwrap();
    // Cargo no longer reports the deleted script, so the `**/build.rs` rule must catch it.
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
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
    let lib = json["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "crates/lonely/src/lib.rs")
        .unwrap();
    assert_eq!(lib["effect"], "outside");
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
    // The shim could depend on any member, so a member change selects the shim's users.
    repo.git(&["checkout", "-q", "--", "."]);
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    let json = repo.select("HEAD", &[]);
    assert!(selected(&json).contains(&"lonely".to_string()), "{json:#}");
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
    assert_eq!(
        stdout(&output).trim(),
        "--workspace --exclude builder --exclude core --exclude lonely --exclude tester"
    );
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
        "test --workspace --exclude builder --exclude core --exclude lonely --exclude tester --no-fail-fast\n\
         nextest run --workspace --exclude builder --exclude core --exclude lonely --exclude tester --profile ci\n\
         nextest run --workspace\n"
    );
}

#[test]
fn test_runs_cargo_test_only_on_the_selection() {
    let repo = chain();
    // Build output and a new lockfile must not look like changes.
    repo.write(".gitignore", "/target\n");
    let status = isolated(Command::new(env!("CARGO")))
        .args(["generate-lockfile", "--offline"])
        .current_dir(&repo.dir)
        .status()
        .unwrap();
    assert!(status.success());
    repo.append(
        "crates/core/src/lib.rs",
        "#[test]\nfn core_works() { assert_eq!(f(), 1); }\n",
    );
    repo.append(
        "crates/lonely/src/lib.rs",
        "#[test]\nfn lonely_fails() { panic!(\"lonely is broken\"); }\n",
    );
    repo.append(
        "crates/mid/src/lib.rs",
        "#[test]\nfn mid_runs() { assert_eq!(f(), 1); }\n",
    );
    repo.commit("tests");

    repo.append("crates/mid/src/lib.rs", "// changed\n");
    let output = repo.tia(&["tia", "test", "--base", "HEAD"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("test mid_runs ... ok"),
        "{}",
        stdout(&output)
    );
    assert!(
        !stdout(&output).contains("core_works"),
        "core is not selected"
    );
    assert!(
        stderr(&output).contains("selected: 2 of 6 packages"),
        "{}",
        stderr(&output)
    );

    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    let output = repo.tia(&["tia", "test", "--base", "HEAD"]);
    assert_eq!(output.status.code(), Some(101), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("selected: 3 of 6 packages"),
        "{}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("lonely is broken")
            || stderr(&output).contains("lonely is broken")
    );
}

#[test]
fn missing_origin_head_selects_the_whole_workspace() {
    let origin = chain();
    let parent = Repo::new("nohead");
    let status = isolated(Command::new("git"))
        .args(["clone", "-q"])
        .arg(&origin.dir)
        .arg(parent.path("work"))
        .status()
        .unwrap();
    assert!(status.success());
    let clone = Repo {
        dir: parent.path("work"),
    };
    clone.git(&["remote", "set-head", "origin", "-d"]);
    clone.append("crates/app/src/lib.rs", "pub fn g() {}\n");
    let output = clone.tia(&["list", "--json"]);
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(is_full(&json), "no guess at origin/main: {json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("origin/HEAD"));
    assert_eq!(selected(&clone.select("origin/main", &[])), ["app"]);
}

#[test]
fn several_merge_bases_select_the_whole_workspace() {
    let repo = chain();
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.append("crates/app/src/lib.rs", "// side\n");
    let side = repo.commit("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.append("crates/core/src/lib.rs", "// main\n");
    let main = repo.commit("main");
    // Criss-cross: each branch merges the other's first commit.
    repo.git(&["merge", "-q", "--no-ff", "--no-edit", &side]);
    repo.git(&["checkout", "-q", "side"]);
    repo.git(&["merge", "-q", "--no-ff", "--no-edit", &main]);
    let json = repo.select("main", &[]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("2 merge bases"));
}

#[test]
fn committed_only_needs_committed_manifests() {
    let repo = chain();
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    // The workspace graph comes from the working tree, which no longer matches HEAD.
    repo.append("crates/app/Cargo.toml", "\n");
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("uncommitted"));
    // Other uncommitted files do not matter with --committed.
    repo.git(&["checkout", "-q", "--", "."]);
    repo.write("notes.txt", "notes\n");
    let json = repo.select("HEAD~1", &["--committed"]);
    assert_eq!(selected(&json), ["app", "builder", "core", "mid", "tester"]);
}

#[cfg(unix)]
#[test]
fn symlinked_files_select_the_package_that_holds_the_link() {
    let repo = chain();
    std::os::unix::fs::symlink(
        "../../lonely/src/lib.rs",
        repo.path("crates/app/src/shared.rs"),
    )
    .unwrap();
    repo.commit("link");
    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert_eq!(selected(&json), ["app", "lonely"], "{json:#}");
    let link = json["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "crates/app/src/shared.rs")
        .unwrap();
    assert_eq!(link["link_to"], "crates/lonely/src/lib.rs");
}

#[test]
fn assume_unchanged_files_still_count() {
    let repo = chain();
    repo.git(&[
        "update-index",
        "--assume-unchanged",
        "crates/lonely/src/lib.rs",
    ]);
    repo.append("crates/lonely/src/lib.rs", "// hidden from git diff\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[test]
fn submodule_changes_count_even_when_git_config_hides_them() {
    let sub = Repo::new("sub");
    sub.write("data.txt", "1\n");
    sub.commit("data");
    let repo = chain();
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        sub.dir.to_str().unwrap(),
        "crates/lonely/vendor",
    ]);
    repo.commit("submodule");
    repo.git(&["config", "diff.ignoreSubmodules", "all"]);
    // A new commit inside the submodule moves its gitlink.
    let inner = Repo {
        dir: repo.path("crates/lonely/vendor"),
    };
    inner.append("data.txt", "2\n");
    inner.commit("bump");
    // The submodule could hold a Cargo.toml or a build script, so its contents are unknown.
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert_eq!(json["changed_files"][0]["path"], "crates/lonely/vendor");
}

#[cfg(unix)]
#[test]
fn changed_symlinks_select_the_whole_workspace() {
    let repo = chain();
    fs::create_dir_all(repo.path("crates/lonely/src/v1")).unwrap();
    fs::create_dir_all(repo.path("crates/lonely/src/v2")).unwrap();
    std::os::unix::fs::symlink("v1", repo.path("crates/lonely/src/current")).unwrap();
    repo.write("crates/lonely/src/v1/x.rs", "// v1\n");
    repo.write("crates/lonely/src/v2/x.rs", "// v2\n");
    repo.commit("versions");
    // Retargeting a link changes what every reader of the link compiles.
    fs::remove_file(repo.path("crates/lonely/src/current")).unwrap();
    std::os::unix::fs::symlink("v2", repo.path("crates/lonely/src/current")).unwrap();
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    repo.commit("retarget");
    assert!(is_full(&repo.select("HEAD~1", &["--committed"])));
    // A new, untracked link too.
    std::os::unix::fs::symlink("v1", repo.path("crates/lonely/src/old")).unwrap();
    assert!(is_full(&repo.select("HEAD", &[])));
}

#[test]
fn skip_worktree_edits_still_count() {
    let repo = chain();
    repo.git(&[
        "update-index",
        "--skip-worktree",
        "crates/lonely/src/lib.rs",
    ]);
    repo.append("crates/lonely/src/lib.rs", "// hidden from git diff\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[test]
fn committed_only_distrusts_hidden_manifests() {
    let repo = chain();
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    repo.git(&[
        "update-index",
        "--assume-unchanged",
        "crates/app/Cargo.toml",
    ]);
    // Unchanged, the hidden manifest is fine.
    assert!(!is_full(&repo.select("HEAD~1", &["--committed"])));
    // Edited behind git's back, it is not.
    repo.append("crates/app/Cargo.toml", "\n");
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn files_next_to_a_target_of_another_package_select_the_whole_workspace() {
    let repo = chain();
    let manifest = fs::read_to_string(repo.path("crates/app/Cargo.toml")).unwrap();
    repo.write(
        "crates/app/Cargo.toml",
        &(manifest + "\n[[test]]\nname = \"check\"\npath = \"../lonely/checks/check.rs\"\n"),
    );
    repo.write(
        "crates/lonely/checks/check.rs",
        "mod helper;\n#[test] fn t() {}\n",
    );
    repo.write("crates/lonely/checks/helper.rs", "pub fn h() {}\n");
    repo.commit("cross target");
    // A module of the target, not the target file itself.
    repo.append("crates/lonely/checks/helper.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "app compiles this file: {json:#}");
    // Other files of lonely still select lonely only.
    repo.git(&["checkout", "-q", "--", "."]);
    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[test]
fn path_dependency_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-crate");
    outside.write(
        "Cargo.toml",
        "[package]\nname = \"outer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    outside.write("src/lib.rs", "pub fn o() {}\n");
    let repo = chain();
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "[dependencies]\n",
            &format!(
                "[dependencies]\nouter = {{ path = {:?} }}\n",
                outside.dir.display().to_string()
            ),
        ),
    );
    repo.commit("outer dependency");
    // Git cannot see an edit in the other directory, so nothing can be ruled out.
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("outside the git repository"));
}

#[test]
fn a_patch_makes_registry_users_depend_on_every_member() {
    let repo = chain();
    repo.append(
        "Cargo.toml",
        "\n[patch.crates-io]\ncore = { path = \"crates/core\" }\n",
    );
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace("[dependencies]\n", "[dependencies]\nwrapper = \"1\"\n"),
    );
    repo.commit("patch");
    // A registry `wrapper` may depend on the patched `core`.
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    let json = repo.select("HEAD", &[]);
    assert!(selected(&json).contains(&"lonely".to_string()), "{json:#}");
    // No patch points to `mid`, so its change does not reach `lonely`.
    repo.git(&["checkout", "-q", "--", "."]);
    repo.append("crates/mid/src/lib.rs", "pub fn g() {}\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["app", "mid"]);
}

#[test]
fn commented_patches_change_nothing() {
    let repo = chain();
    repo.append(
        "Cargo.toml",
        "\n[patch.crates-io]\n# core = { path = \"crates/core\" }\n",
    );
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace("[dependencies]\n", "[dependencies]\nwrapper = \"1\"\n"),
    );
    repo.commit("commented patch");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    let json = repo.select("HEAD", &[]);
    assert!(!selected(&json).contains(&"lonely".to_string()), "{json:#}");
}

#[test]
fn args_keep_the_manifest_path() {
    let repo = chain();
    repo.append("crates/mid/src/lib.rs", "pub fn g() {}\n");
    let manifest = repo.path("Cargo.toml");
    let manifest = manifest.to_str().unwrap();
    let output = repo.tia_in(
        &std::env::temp_dir(),
        &["args", "--base", "HEAD", "--manifest-path", manifest],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with(&format!("--manifest-path {manifest} --workspace --exclude")),
        "{}",
        stdout(&output)
    );
}

#[test]
fn root_package_test_under_a_member_selects_the_whole_workspace() {
    let repo = Repo::new("roottest");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"rootpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [[test]]\nname = \"shared\"\npath = \"crates/helper/tests/shared.rs\"\n\n\
         [workspace]\nmembers = [\"crates/helper\"]\n",
    );
    repo.write("src/lib.rs", "pub fn r() {}\n");
    repo.write(
        "crates/helper/Cargo.toml",
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("crates/helper/src/lib.rs", "pub fn h() {}\n");
    repo.write("crates/helper/tests/shared.rs", "#[test] fn t() {}\n");
    repo.commit("initial");
    repo.append("crates/helper/tests/shared.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "rootpkg runs this test: {json:#}");
}

/// lonely -> vendor/shim, not a member, with an optional dependency and build script line.
fn with_shim(repo: &Repo, shim_dependency: &str, shim_build: &str) {
    repo.write(
        "vendor/shim/Cargo.toml",
        &format!(
            "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{shim_build}\n\
             [dependencies]\n{shim_dependency}\n"
        ),
    );
    repo.write("vendor/shim/src/lib.rs", "pub fn s() {}\n");
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "[dependencies]\n",
            "[dependencies]\nshim = { path = \"../../vendor/shim\" }\n",
        ),
    );
    repo.commit("shim");
}

#[test]
fn path_dependency_of_a_path_dependency_outside_the_repository() {
    let outside = Repo::new("outside-deep");
    outside.write(
        "Cargo.toml",
        "[package]\nname = \"outer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    outside.write("src/lib.rs", "pub fn o() {}\n");
    let repo = chain();
    with_shim(
        &repo,
        &format!(
            "outer = {{ path = {:?} }}",
            outside.dir.display().to_string()
        ),
        "",
    );
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("outside the git repository"));
}

#[test]
fn custom_build_script_of_a_path_dependency_selects_the_whole_workspace() {
    let repo = chain();
    with_shim(&repo, "", "build = \"generate.rs\"");
    repo.write("vendor/shim/generate.rs", "fn main() {}\n");
    repo.commit("generator");
    repo.append("vendor/shim/generate.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    // Other shim files select the shim's users only.
    repo.git(&["checkout", "-q", "--", "."]);
    repo.append("vendor/shim/src/lib.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[test]
fn nested_path_dependency_changes_select_the_users() {
    let repo = chain();
    repo.write(
        "vendor/inner/Cargo.toml",
        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("vendor/inner/src/lib.rs", "pub fn i() {}\n");
    with_shim(&repo, "inner = { path = \"../inner\" }", "");
    repo.append("vendor/inner/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(!is_full(&json), "{json:#}");
    assert_eq!(selected(&json), ["lonely"]);
}

#[cfg(unix)]
#[test]
fn config_override_for_the_test_command_selects_the_whole_workspace() {
    let repo = chain();
    fs::write(
        repo.path(".git/info/exclude"),
        "fake-cargo.sh\ncargo-calls.log\n",
    )
    .unwrap();
    let cargo = fake_cargo(&repo);
    repo.append("crates/mid/src/lib.rs", "pub fn g() {}\n");
    let output = isolated(Command::new(BIN))
        .current_dir(&repo.dir)
        .env("CARGO", &cargo)
        .args([
            "tia",
            "test",
            "--base",
            "HEAD",
            "--",
            "--config",
            "build.rustflags=['-Dwarnings']",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let output = isolated(Command::new(BIN))
        .current_dir(&repo.dir)
        .env("CARGO", &cargo)
        .args([
            "tia",
            "nextest",
            "--base",
            "HEAD",
            "--",
            "--config-file=ci/nextest.toml",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let calls = fs::read_to_string(repo.path("cargo-calls.log")).unwrap();
    assert_eq!(
        calls,
        "test --workspace --config build.rustflags=['-Dwarnings']\n\
         nextest run --workspace --config-file=ci/nextest.toml\n"
    );
}

#[test]
fn local_patches_that_are_not_members() {
    let repo = chain();
    repo.write(
        "vendor/widget/Cargo.toml",
        "[package]\nname = \"widget\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    );
    repo.write("vendor/widget/src/lib.rs", "pub fn w() {}\n");
    repo.append(
        "Cargo.toml",
        "exclude = [\"vendor\"]\n\n[patch.crates-io]\nwidget = { path = \"vendor/widget\" }\n",
    );
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "[dependencies]\n",
            "[dependencies]\nregistry-user = \"1\"\n",
        ),
    );
    repo.commit("patch to a local crate");
    // A registry dependency of lonely can resolve to the patched widget.
    repo.append("vendor/widget/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(!is_full(&json), "{json:#}");
    assert_eq!(selected(&json), ["lonely"]);
}

#[test]
fn local_patch_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-patch");
    outside.write(
        "Cargo.toml",
        "[package]\nname = \"widget\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    );
    outside.write("src/lib.rs", "pub fn w() {}\n");
    let repo = chain();
    repo.append(
        "Cargo.toml",
        &format!(
            "\n[patch.crates-io]\nwidget = {{ path = {:?} }}\n",
            outside.dir.display().to_string()
        ),
    );
    repo.commit("patch to a sibling checkout");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("outside the git repository"));
}

#[test]
fn member_test_inside_a_path_dependency_selects_the_whole_workspace() {
    let repo = chain();
    repo.write(
        "crates/app/vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("crates/app/vendor/shim/src/lib.rs", "pub fn s() {}\n");
    repo.write(
        "crates/app/vendor/shim/tests/smoke.rs",
        "#[test] fn t() {}\n",
    );
    repo.append("Cargo.toml", "exclude = [\"crates/app/vendor\"]\n");
    let app = fs::read_to_string(repo.path("crates/app/Cargo.toml")).unwrap();
    repo.write(
        "crates/app/Cargo.toml",
        &(app + "\n[[test]]\nname = \"smoke\"\npath = \"vendor/shim/tests/smoke.rs\"\n"),
    );
    let lonely = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &lonely.replace(
            "[dependencies]\n",
            "[dependencies]\nshim = { path = \"../app/vendor/shim\" }\n",
        ),
    );
    repo.commit("shared test");
    // The shim owns the file, but app compiles it as a test.
    repo.append("crates/app/vendor/shim/tests/smoke.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[cfg(unix)]
#[test]
fn file_system_monitor_hooks_do_not_run() {
    use std::os::unix::fs::PermissionsExt;
    let repo = chain();
    let marker = repo.path("hook-ran");
    let hook = repo.path(".git/fsmonitor-hook.sh");
    fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    repo.git(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
    assert!(!marker.exists(), "the fsmonitor hook ran");
}

/// Writes a non-member crate at `dir` with path dependencies on the given directories.
fn vendor_crate(repo: &Repo, dir: &str, deps: &[&str]) {
    let name = dir.rsplit('/').next().unwrap();
    let mut manifest = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n"
    );
    for dep in deps {
        let dep_name = dep.rsplit('/').next().unwrap();
        manifest += &format!("{dep_name} = {{ path = \"{dep}\" }}\n");
    }
    repo.write(&format!("{dir}/Cargo.toml"), &manifest);
    repo.write(&format!("{dir}/src/lib.rs"), "pub fn v() {}\n");
}

fn use_vendor_crate(repo: &Repo, member: &str, name: &str) {
    let path = format!("crates/{member}/Cargo.toml");
    let manifest = fs::read_to_string(repo.path(&path)).unwrap();
    repo.write(
        &path,
        &manifest.replace(
            "[dependencies]\n",
            &format!("[dependencies]\n{name} = {{ path = \"../../vendor/{name}\" }}\n"),
        ),
    );
}

#[test]
fn shared_nested_path_dependency_selects_every_user() {
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    // one and two share common, and common uses base: base must reach both users.
    vendor_crate(&repo, "vendor/base", &[]);
    vendor_crate(&repo, "vendor/common", &["../base"]);
    vendor_crate(&repo, "vendor/one", &["../common"]);
    vendor_crate(&repo, "vendor/two", &["../common"]);
    use_vendor_crate(&repo, "lonely", "one");
    use_vendor_crate(&repo, "builder", "two");
    repo.commit("vendored crates");
    repo.append("vendor/common/src/lib.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["builder", "lonely"]);
    repo.git(&["checkout", "-q", "--", "."]);
    repo.append("vendor/base/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert_eq!(selected(&json), ["builder", "lonely"]);
    // The report names every user of base, not only the first one found.
    assert_eq!(
        json["changed_files"][0]["packages"],
        serde_json::json!(["builder", "lonely"])
    );
}

#[test]
fn cyclic_path_dependencies_terminate() {
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    vendor_crate(&repo, "vendor/x", &["../y"]);
    vendor_crate(&repo, "vendor/y", &["../x"]);
    use_vendor_crate(&repo, "lonely", "x");
    repo.commit("cycle");
    repo.append("vendor/y/src/lib.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[test]
fn path_override_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-override");
    vendor_crate(&outside, "local-uuid", &[]);
    let repo = chain();
    repo.write(
        ".cargo/config.toml",
        &format!(
            "paths = [{:?}]\n",
            outside.path("local-uuid").display().to_string()
        ),
    );
    repo.commit("paths override");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn target_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-target");
    outside.write("check.rs", "#[test] fn t() {}\n");
    let repo = chain();
    let app = fs::read_to_string(repo.path("crates/app/Cargo.toml")).unwrap();
    repo.write(
        "crates/app/Cargo.toml",
        &format!(
            "{app}\n[[test]]\nname = \"check\"\npath = {:?}\n",
            outside.path("check.rs").display().to_string()
        ),
    );
    repo.commit("outside test");
    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    // The report keeps the changed files.
    assert_eq!(json["changed_files"][0]["path"], "crates/lonely/src/lib.rs");
}

#[cfg(unix)]
#[test]
fn link_to_the_workspace_root_reaches_every_file() {
    let repo = chain();
    std::os::unix::fs::symlink("../../..", repo.path("crates/app/src/everything")).unwrap();
    repo.commit("root link");
    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert_eq!(selected(&json), ["app", "lonely"], "{json:#}");
}

#[test]
fn local_patch_used_through_a_path_dependency() {
    let repo = chain();
    repo.append(
        "Cargo.toml",
        "exclude = [\"vendor\"]\n\n[patch.crates-io]\nwidget = { path = \"vendor/widget\" }\n",
    );
    vendor_crate(&repo, "vendor/widget", &[]);
    // shim, not a member, uses a registry package that can resolve to widget.
    repo.write(
        "vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nwidget-user = \"1\"\n",
    );
    repo.write("vendor/shim/src/lib.rs", "pub fn s() {}\n");
    use_vendor_crate(&repo, "lonely", "shim");
    repo.commit("patch used through shim");
    repo.append("vendor/widget/src/lib.rs", "// changed\n");
    assert!(selected(&repo.select("HEAD", &[])).contains(&"lonely".to_string()));
}

#[test]
fn deleted_skip_worktree_files_count_without_sparse_checkout() {
    let repo = chain();
    repo.write("crates/lonely/src/parser.rs", "pub fn p() {}\n");
    repo.commit("parser");
    repo.git(&[
        "update-index",
        "--skip-worktree",
        "crates/lonely/src/parser.rs",
    ]);
    fs::remove_file(repo.path("crates/lonely/src/parser.rs")).unwrap();
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
    // In a sparse checkout, missing skip-worktree files are expected.
    repo.git(&["config", "core.sparseCheckout", "true"]);
    assert!(selected(&repo.select("HEAD", &[])).is_empty());
}

#[test]
fn path_dependency_target_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-lib");
    outside.write("lib.rs", "pub fn o() {}\n");
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    repo.write(
        "vendor/shim/Cargo.toml",
        &format!(
            "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = {:?}\n",
            outside.path("lib.rs").display().to_string()
        ),
    );
    use_vendor_crate(&repo, "lonely", "shim");
    repo.commit("shim with an outside lib");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[cfg(unix)]
#[test]
fn symlink_to_a_file_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-link");
    outside.write("generated.rs", "pub fn g() {}\n");
    let repo = chain();
    std::os::unix::fs::symlink(
        outside.path("generated.rs"),
        repo.path("crates/app/src/generated.rs"),
    )
    .unwrap();
    repo.commit("outside link");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn quoted_build_key_of_a_path_dependency_selects_the_whole_workspace() {
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    repo.write(
        "vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\"build\" = \"generate.rs\"\n",
    );
    repo.write("vendor/shim/src/lib.rs", "pub fn s() {}\n");
    repo.write("vendor/shim/generate.rs", "fn main() {}\n");
    use_vendor_crate(&repo, "lonely", "shim");
    repo.commit("quoted build key");
    repo.append("vendor/shim/generate.rs", "// changed\n");
    assert!(is_full(&repo.select("HEAD", &[])));
}

#[test]
fn patches_in_included_config_files_count() {
    let repo = chain();
    repo.write(".cargo/config.toml", "include = [\"patches.toml\"]\n");
    repo.write(
        ".cargo/patches.toml",
        "[patch.crates-io]\ncore = { path = \"crates/core\" }\n",
    );
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace("[dependencies]\n", "[dependencies]\nwrapper = \"1\"\n"),
    );
    repo.commit("included patch");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    assert!(selected(&repo.select("HEAD", &[])).contains(&"lonely".to_string()));
}

#[test]
fn source_replacement_directories_select_the_whole_workspace() {
    let repo = chain();
    repo.write(
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = \"crates/lonely/vendor\"\n",
    );
    repo.write("crates/lonely/vendor/widget/src/lib.rs", "pub fn w() {}\n");
    repo.commit("vendored sources");
    // Any member could use the vendored crate, not only the package that holds the directory.
    repo.append("crates/lonely/vendor/widget/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn path_dependency_target_inside_a_member_reaches_the_users() {
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    repo.write("crates/core/src/compat.rs", "pub fn c() {}\n");
    repo.write(
        "vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"../../crates/core/src/compat.rs\"\n",
    );
    use_vendor_crate(&repo, "lonely", "shim");
    repo.commit("shim library inside core");
    // The file belongs to core; lonely uses it through the shim.
    repo.append("crates/core/src/compat.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    let reason = json["full_reasons"][0].as_str().unwrap();
    assert!(
        reason.contains("next to a target of `vendor/shim`"),
        "{reason}"
    );
}

#[test]
fn included_config_files_select_the_whole_workspace() {
    let repo = chain();
    repo.write(
        "crates/app/.cargo/config.toml",
        "include = [\"patches.toml\"]\n",
    );
    repo.write("crates/app/.cargo/patches.toml", "[env]\nX = \"1\"\n");
    repo.commit("member config");
    repo.append("crates/app/.cargo/patches.toml", "Y = \"2\"\n");
    let output = repo.tia_in(
        &repo.path("crates/app"),
        &["list", "--json", "--base", "HEAD"],
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn nested_repositories_select_the_whole_workspace() {
    let repo = chain();
    let nested = Repo {
        dir: repo.path("crates/lonely/vendor/shim"),
    };
    fs::create_dir_all(&nested.dir).unwrap();
    nested.git(&["init", "-q", "-b", "main"]);
    nested.write("src/lib.rs", "pub fn s() {}\n");
    nested.commit("independent checkout");
    // Git lists the checkout as one directory entry and hides its files.
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[cfg(unix)]
#[test]
fn executable_bit_changes_count_even_when_git_ignores_them() {
    use std::os::unix::fs::PermissionsExt;
    let repo = chain();
    repo.write("crates/lonely/tests/run.sh", "#!/bin/sh\n");
    fs::set_permissions(
        repo.path("crates/lonely/tests/run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    repo.commit("script");
    repo.git(&["config", "core.fileMode", "false"]);
    fs::set_permissions(
        repo.path("crates/lonely/tests/run.sh"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[test]
fn inherited_path_dependency_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-inherited");
    vendor_crate(&outside, "outer", &[]);
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"other\"]\n");
    // `other` is a second workspace; its member `shim` inherits `outer` from it.
    repo.write(
        "other/Cargo.toml",
        &format!(
            "[workspace]\nmembers = [\"shim\"]\n\n[workspace.dependencies]\nouter = {{ path = {:?} }}\n",
            outside.path("outer").display().to_string()
        ),
    );
    repo.write(
        "other/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nouter.workspace = true\n",
    );
    repo.write("other/shim/src/lib.rs", "pub fn s() {}\n");
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "[dependencies]\n",
            "[dependencies]\nshim = { path = \"../../other/shim\" }\n",
        ),
    );
    repo.commit("inherited dependency");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("outside the git repository"));
}

#[test]
fn paths_in_included_config_files_start_from_their_own_directory() {
    let repo = chain();
    repo.write(
        ".cargo/config.toml",
        "include = [\"../crates/lonely/.cargo/vendor.toml\"]\n",
    );
    repo.write(
        "crates/lonely/.cargo/vendor.toml",
        "[source.vendored]\ndirectory = \"vendor\"\n",
    );
    repo.write("crates/lonely/vendor/widget/src/lib.rs", "pub fn w() {}\n");
    repo.commit("included vendor config");
    repo.append("crates/lonely/vendor/widget/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn included_config_outside_the_repository_selects_the_whole_workspace() {
    let outside = Repo::new("outside-config");
    outside.write("settings.toml", "[env]\nX = \"1\"\n");
    let repo = chain();
    repo.write(
        ".cargo/config.toml",
        &format!(
            "include = [{:?}]\n",
            outside.path("settings.toml").display().to_string()
        ),
    );
    repo.commit("shared settings");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[cfg(unix)]
#[test]
fn unreadable_directories_select_the_whole_workspace() {
    use std::os::unix::fs::PermissionsExt;
    let repo = chain();
    let locked = repo.path("crates/lonely/data");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("input.txt"), "data\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&locked).is_ok(); // true when the tests run as root
    let json = repo.select("HEAD", &[]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if !readable {
        assert!(
            is_full(&json),
            "git cannot list the untracked files: {json:#}"
        );
    }
}

#[test]
fn member_inside_another_package_source_directory() {
    let repo = Repo::new("nested-member");
    repo.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"app/src/engine\"]\n",
    );
    repo.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write("app/src/lib.rs", "mod engine;\n");
    repo.write(
        "app/src/engine/Cargo.toml",
        "[package]\nname = \"engine\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"mod.rs\"\n",
    );
    repo.write("app/src/engine/mod.rs", "pub fn e() {}\n");
    repo.commit("nested member");
    // app compiles engine/mod.rs as a module, without a Cargo dependency on engine.
    repo.append("app/src/engine/mod.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["app", "engine"]);
}

#[cfg(unix)]
#[test]
fn hidden_symlink_changes_select_the_whole_workspace() {
    let repo = chain();
    repo.write("crates/lonely/src/v1.rs", "// v1\n");
    repo.write("crates/lonely/src/v2.rs", "// v2\n");
    std::os::unix::fs::symlink("v1.rs", repo.path("crates/lonely/src/current.rs")).unwrap();
    repo.commit("versions");
    repo.git(&[
        "update-index",
        "--assume-unchanged",
        "crates/lonely/src/current.rs",
    ]);
    fs::remove_file(repo.path("crates/lonely/src/current.rs")).unwrap();
    std::os::unix::fs::symlink("v2.rs", repo.path("crates/lonely/src/current.rs")).unwrap();
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn named_workspace_of_a_path_dependency() {
    let outside = Repo::new("outside-named");
    vendor_crate(&outside, "outer", &[]);
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"other\", \"vendor\"]\n");
    // `vendor/shim` names `other` as its workspace; `other` is not one of its ancestors.
    repo.write(
        "other/Cargo.toml",
        &format!(
            "[workspace]\nmembers = [\"../vendor/shim\"]\n\n[workspace.dependencies]\nouter = {{ path = {:?} }}\n",
            outside.path("outer").display().to_string()
        ),
    );
    repo.write(
        "vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\nworkspace = \"../../other\"\n\n[dependencies]\nouter.workspace = true\n",
    );
    repo.write("vendor/shim/src/lib.rs", "pub fn s() {}\n");
    use_vendor_crate(&repo, "lonely", "shim");
    repo.commit("named workspace");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
    assert!(json["full_reasons"][0]
        .as_str()
        .unwrap()
        .contains("outside the git repository"));
}

#[test]
fn case_only_renames_count() {
    let repo = chain();
    repo.write("crates/lonely/src/worker.rs", "// worker\n");
    repo.commit("worker");
    // On a case-insensitive file system git hides this rename unless ignoreCase is off.
    repo.git(&["config", "core.ignoreCase", "true"]);
    fs::rename(
        repo.path("crates/lonely/src/worker.rs"),
        repo.path("crates/lonely/src/Worker.rs"),
    )
    .unwrap();
    let json = repo.select("HEAD", &[]);
    assert_eq!(selected(&json), ["lonely"]);
    // Both spellings count, so a glob for the old one still applies.
    let json = repo.select("HEAD", &["--full-run", "crates/lonely/src/worker.rs"]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn recursive_paths_override_covers_nested_members() {
    let repo = chain();
    // `paths = ["."]` overrides registry packages with every crate below the root.
    repo.write(".cargo/config.toml", "paths = [\".\"]\n");
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace("[dependencies]\n", "[dependencies]\nwrapper = \"1\"\n"),
    );
    repo.commit("paths override");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    assert!(selected(&repo.select("HEAD", &[])).contains(&"lonely".to_string()));
}

#[test]
fn path_dependency_target_owned_by_another_path_dependency() {
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"vendor\"]\n");
    vendor_crate(&repo, "vendor/shared", &[]);
    repo.write(
        "vendor/shim/Cargo.toml",
        "[package]\nname = \"shim\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"../shared/src/lib.rs\"\n",
    );
    use_vendor_crate(&repo, "lonely", "shim");
    use_vendor_crate(&repo, "builder", "shared");
    repo.commit("shared library file");
    repo.append("vendor/shared/src/lib.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "shim compiles shared's file: {json:#}");
}

#[test]
fn committed_only_distrusts_uncommitted_cargo_configuration() {
    let repo = chain();
    repo.write(".cargo/config.toml", "[build]\njobs = 2\n");
    repo.commit("config");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    repo.write(".cargo/config.toml", "[build]\njobs = 4\n");
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn crate_inside_a_member_source_directory_that_another_member_uses() {
    let repo = chain();
    repo.append("Cargo.toml", "exclude = [\"crates/app/src/engine\"]\n");
    // app compiles engine/mod.rs as a module; engine is also a non-member crate of lonely.
    repo.write("crates/app/src/lib.rs", "mod engine;\n");
    repo.write(
        "crates/app/src/engine/Cargo.toml",
        "[package]\nname = \"engine\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"mod.rs\"\n",
    );
    repo.write("crates/app/src/engine/mod.rs", "pub fn e() {}\n");
    let manifest = fs::read_to_string(repo.path("crates/lonely/Cargo.toml")).unwrap();
    repo.write(
        "crates/lonely/Cargo.toml",
        &manifest.replace(
            "[dependencies]\n",
            "[dependencies]\nengine = { path = \"../app/src/engine\" }\n",
        ),
    );
    repo.commit("engine");
    repo.append("crates/app/src/engine/mod.rs", "// changed\n");
    let json = repo.select("HEAD", &[]);
    // Cargo 1.99 makes `engine` a member despite `exclude`, so the nested-member rule applies.
    // A unit test in workspace.rs covers the non-member case.
    assert_eq!(selected(&json), ["app", "engine", "lonely"], "{json:#}");
}

#[test]
fn committed_only_distrusts_included_configuration() {
    let repo = chain();
    repo.write(".cargo/config.toml", "include = [\"../ci/patches.toml\"]\n");
    repo.write("ci/patches.toml", "[env]\nX = \"1\"\n");
    repo.commit("config");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    repo.write("ci/patches.toml", "[env]\nX = \"2\"\n");
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
}

#[cfg(unix)]
#[test]
fn committed_only_distrusts_changed_links() {
    let repo = chain();
    std::os::unix::fs::symlink(
        "../../core/src/lib.rs",
        repo.path("crates/lonely/src/core.rs"),
    )
    .unwrap();
    repo.commit("link");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    fs::remove_file(repo.path("crates/lonely/src/core.rs")).unwrap();
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn weak_stat_settings_cannot_hide_edits() {
    let repo = chain();
    repo.git(&["config", "core.trustctime", "false"]);
    repo.git(&["config", "core.checkStat", "minimal"]);
    // Let the index become newer than the files, so git trusts their stat data.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    repo.git(&["update-index", "--really-refresh"]);
    repo.git(&["status", "--porcelain"]);
    let path = repo.path("crates/lonely/src/lib.rs");
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    let text = fs::read_to_string(&path).unwrap();
    // Same size, same modification time, other bytes.
    fs::write(&path, text.replace('1', "2")).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(before)
        .unwrap();
    assert_eq!(selected(&repo.select("HEAD", &[])), ["lonely"]);
}

#[cfg(unix)]
#[test]
fn committed_only_distrusts_hidden_link_edits() {
    let repo = chain();
    std::os::unix::fs::symlink(
        "../../core/src/lib.rs",
        repo.path("crates/lonely/src/core.rs"),
    )
    .unwrap();
    repo.commit("link");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    repo.git(&[
        "update-index",
        "--skip-worktree",
        "crates/lonely/src/core.rs",
    ]);
    fs::remove_file(repo.path("crates/lonely/src/core.rs")).unwrap();
    std::os::unix::fs::symlink(
        "../../mid/src/lib.rs",
        repo.path("crates/lonely/src/core.rs"),
    )
    .unwrap();
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn member_inside_a_build_script_source_directory() {
    let repo = Repo::new("build-modules");
    repo.write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"gen\", \"gen/build/backend\"]\n",
    );
    repo.write(
        "gen/Cargo.toml",
        "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build/main.rs\"\n",
    );
    repo.write("gen/src/lib.rs", "pub fn g() {}\n");
    repo.write("gen/build/main.rs", "mod backend;\nfn main() {}\n");
    repo.write(
        "gen/build/backend/Cargo.toml",
        "[package]\nname = \"backend\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"mod.rs\"\n",
    );
    repo.write("gen/build/backend/mod.rs", "pub fn b() {}\n");
    repo.commit("build script modules");
    // gen's build script compiles backend/mod.rs as a module.
    repo.append("gen/build/backend/mod.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["backend", "gen"]);
}

#[cfg(unix)]
#[test]
fn includes_resolve_from_a_symlink_location() {
    let repo = chain();
    repo.write("shared/shared.toml", "include = [\"settings.toml\"]\n");
    std::fs::create_dir_all(repo.path("crates/app/.cargo")).unwrap();
    std::os::unix::fs::symlink(
        "../../../shared/shared.toml",
        repo.path("crates/app/.cargo/shared.toml"),
    )
    .unwrap();
    repo.write(
        "crates/app/.cargo/config.toml",
        "include = [\"shared.toml\"]\n",
    );
    // Cargo looks for settings.toml beside the link, not beside its target.
    repo.write("crates/app/.cargo/settings.toml", "[env]\nX = \"1\"\n");
    repo.commit("linked include");
    repo.write("crates/app/.cargo/settings.toml", "[env]\nX = \"2\"\n");
    let output = repo.tia_in(
        &repo.path("crates/app"),
        &["list", "--json", "--base", "HEAD"],
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn member_inside_a_root_build_script_directory() {
    let repo = Repo::new("root-codegen");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"rootpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"codegen/main.rs\"\n\n[workspace]\nmembers = [\"codegen/backend\"]\n",
    );
    repo.write("src/lib.rs", "pub fn r() {}\n");
    repo.write("codegen/main.rs", "mod backend;\nfn main() {}\n");
    repo.write(
        "codegen/backend/Cargo.toml",
        "[package]\nname = \"backend\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"mod.rs\"\n",
    );
    repo.write("codegen/backend/mod.rs", "pub fn b() {}\n");
    repo.commit("root build script modules");
    repo.append("codegen/backend/mod.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["backend", "rootpkg"]);
}

#[test]
fn root_build_script_modules_in_a_member() {
    let repo = Repo::new("root-build-modules");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"rootpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"codegen\"]\n",
    );
    repo.write("src/lib.rs", "pub fn r() {}\n");
    repo.write("build.rs", "mod codegen;\nfn main() {}\n");
    repo.write(
        "codegen/Cargo.toml",
        "[package]\nname = \"codegen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"mod.rs\"\n",
    );
    repo.write("codegen/mod.rs", "pub fn c() {}\n");
    repo.commit("root build script modules");
    repo.append("codegen/mod.rs", "// changed\n");
    assert_eq!(selected(&repo.select("HEAD", &[])), ["codegen", "rootpkg"]);
}

#[test]
fn cargo_home_configuration_inside_the_repository_selects_the_whole_workspace() {
    let repo = chain();
    repo.write("crates/app/home/config.toml", "[env]\nX = \"1\"\n");
    repo.commit("cargo home in the repository");
    repo.write("crates/app/home/config.toml", "[env]\nX = \"2\"\n");
    let output = isolated(Command::new(BIN))
        .current_dir(&repo.dir)
        .env("CARGO_HOME", repo.path("crates/app/home"))
        .args(["list", "--json", "--base", "HEAD"])
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(is_full(&json), "{json:#}");
}

#[cfg(unix)]
#[test]
fn committed_only_distrusts_the_file_behind_a_linked_config() {
    let repo = chain();
    repo.write("ci/cargo.toml", "[build]\njobs = 2\n");
    std::fs::create_dir_all(repo.path(".cargo")).unwrap();
    std::os::unix::fs::symlink("../ci/cargo.toml", repo.path(".cargo/config.toml")).unwrap();
    repo.commit("linked config");
    repo.append("crates/core/src/lib.rs", "pub fn g() {}\n");
    repo.commit("core");
    repo.write("ci/cargo.toml", "[build]\njobs = 4\n");
    let json = repo.select("HEAD~1", &["--committed"]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn includes_from_cargo_home_are_environment() {
    let home = Repo::new("cargo-home-include");
    home.write("config.toml", "include = [\"extra.toml\"]\n");
    home.write("extra.toml", "[env]\nX = \"1\"\n");
    let repo = chain();
    repo.append("crates/lonely/src/lib.rs", "// changed\n");
    let output = isolated(Command::new(BIN))
        .current_dir(&repo.dir)
        .env("CARGO_HOME", &home.dir)
        .args(["list", "--json", "--base", "HEAD"])
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected(&json), ["lonely"], "{json:#}");
}

#[test]
fn deleted_optional_include_selects_the_whole_workspace() {
    let repo = chain();
    repo.write(
        ".cargo/config.toml",
        "include = [{ path = \"../crates/lonely/test-env.toml\", optional = true }]\n",
    );
    repo.write("crates/lonely/test-env.toml", "[env]\nMODE = \"fast\"\n");
    repo.commit("optional include");
    fs::remove_file(repo.path("crates/lonely/test-env.toml")).unwrap();
    let json = repo.select("HEAD", &[]);
    assert!(is_full(&json), "{json:#}");
}

#[test]
fn hidden_files_without_changes_select_nothing() {
    let repo = chain();
    repo.git(&[
        "update-index",
        "--assume-unchanged",
        "crates/app/Cargo.toml",
    ]);
    repo.git(&["update-index", "--skip-worktree", "crates/mid/src/lib.rs"]);
    let json = repo.select("HEAD", &[]);
    assert!(!is_full(&json), "{json:#}");
    assert!(selected(&json).is_empty(), "{json:#}");
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
