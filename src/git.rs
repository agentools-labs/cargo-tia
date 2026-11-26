//! Changed files from git. Reads the local repository only.

use crate::workspace::{normalize, relative};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Git's repository-local environment variables, as `git rev-parse --local-env-vars` printed
/// them in Git 2.54.
const LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// The repository-local variables to clear, such as GIT_DIR and GIT_INDEX_FILE: the list
/// above and the one the installed Git prints. A Git hook exports them for its own
/// repository, and they would send every command, the ones for submodules too, there. Git
/// clears them itself before it enters a submodule.
fn local_env_vars() -> &'static [String] {
    static VARS: OnceLock<Vec<String>> = OnceLock::new();
    VARS.get_or_init(|| {
        let mut vars: Vec<String> = LOCAL_ENV_VARS.iter().map(|var| var.to_string()).collect();
        let mut command = Command::new("git");
        command.args(["rev-parse", "--local-env-vars"]);
        for var in LOCAL_ENV_VARS {
            command.env_remove(var);
        }
        if let Ok(output) = command.output() {
            let printed = String::from_utf8_lossy(&output.stdout);
            vars.extend(printed.split_whitespace().map(str::to_string));
        }
        vars.sort();
        vars.dedup();
        vars
    })
}

/// The files that differ from the merge base, relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// The base revision as given or detected, for example `origin/main`.
    pub base: String,
    /// The commit that the files are compared with: an explicit base itself, or the merge base
    /// of the default base and `HEAD`.
    pub compared: String,
    /// Changed, added, deleted and (unless committed-only) untracked files.
    pub files: Vec<String>,
    /// Changed files that are or were a symbolic link or a submodule.
    pub special: Vec<String>,
    /// Tracked symbolic links as (link, resolved target), relative to the workspace root.
    pub symlinks: Vec<(String, String)>,
    /// The repository root relative to the workspace root: empty, or `..` segments.
    pub repository: String,
    /// Why the change set may be incomplete although git gave one: the caller selects the full
    /// workspace and still reports the files.
    pub uncertain: Option<String>,
}

/// Why the change set is unknown. The caller then selects the full workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown {
    /// The base revision, when one was given or detected.
    pub base: Option<String>,
    pub reason: String,
}

/// Lists the files that differ from `base`, compared directly: every difference between the two
/// trees counts, also from commits of `base` that `HEAD` lacks, as after a rollback.
///
/// Without `base`, the files that changed since the merge base of `origin/HEAD`, the default
/// branch of `origin`, and `HEAD`. Unless
/// `committed_only` is set, the list includes staged, unstaged and untracked (not ignored)
/// files, and files marked assume-unchanged.
pub fn changed_files(
    workspace_root: &Path,
    base: Option<&str>,
    committed_only: bool,
) -> Result<Changes, Unknown> {
    let fail = |base: Option<&str>, reason: String| Unknown {
        base: base.map(str::to_string),
        reason,
    };
    let toplevel = git(workspace_root, &["rev-parse", "--show-toplevel"])
        .map_err(|error| fail(base, format!("not in a git work tree: {error}")))?;
    let toplevel = normalize(Path::new(toplevel.trim_end_matches(['\n', '\r'])));
    let explicit = base.is_some();
    let base = match base {
        Some(base) => base.to_string(),
        None => default_base(&toplevel).ok_or_else(|| {
            fail(
                None,
                "origin/HEAD is not set: pass --base, or run `git remote set-head origin --auto`"
                    .to_string(),
            )
        })?,
    };
    let fail = |reason: String| fail(Some(&base), reason);
    let merge_base = if explicit {
        // An explicit base is compared directly. A merge base would hide the commits of the base
        // that HEAD lacks: after a rollback, HEAD itself.
        let resolved = git(
            &toplevel,
            // Without --quiet: a warning, such as for an ambiguous name or a reflog date
            // older than the log, makes the base unknown.
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{base}^{{commit}}"),
            ],
        )
        .map_err(|error| fail(format!("cannot resolve `{base}` to a commit: {error}")))?;
        resolved.trim().to_string()
    } else {
        merge_base_with(&toplevel, &base).map_err(fail)?
    };

    let list = |args: &[&str]| -> Result<Vec<String>, Unknown> {
        let output = git(&toplevel, args)
            .map_err(|error| fail(format!("cannot list changed files: {error}")))?;
        Ok(output
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    };
    // Changed paths, and the changed paths that are or were a symbolic link or a submodule.
    let diff = |revisions: &[&str]| -> Result<(Vec<String>, Vec<String>), Unknown> {
        let mut args = vec![
            "diff",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            "--no-ext-diff",
            "--no-color",
            "--ignore-submodules=none",
        ];
        args.extend(revisions);
        args.push("--");
        // Each record is ":<old mode> <new mode> <old object> <new object> <status>", then the path.
        let fields = list(&args)?;
        let (mut paths, mut special) = (Vec::new(), Vec::new());
        for pair in fields.chunks(2) {
            let [meta, path] = pair else {
                return Err(fail("cannot parse `git diff --raw` output".to_string()));
            };
            let mut modes = meta.trim_start_matches(':').split(' ').take(2);
            if modes.any(|mode| mode == "120000" || mode == "160000") {
                special.push(path.clone());
            }
            paths.push(path.clone());
        }
        Ok((paths, special))
    };
    let untracked = || list(&["ls-files", "-z", "--others", "--exclude-standard"]);
    // Each entry is "<tag> <mode> <object> <stage>\t<path>". `git diff` skips the working-tree
    // content of entries marked assume-unchanged (lowercase tag) or skip-worktree (`S`).
    let index = list(&["ls-files", "-z", "-v", "-s"])?;
    // Without a sparse checkout, a missing skip-worktree file was deleted.
    let sparse = git(&toplevel, &["config", "--bool", "core.sparseCheckout"])
        .is_ok_and(|value| value.trim() == "true");
    let mut hidden_links = Vec::new();
    let hidden: Vec<String> = index
        .iter()
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            let mut fields = meta.split(' ');
            let tag = fields.next().unwrap_or_default();
            let mode = fields.next().unwrap_or_default();
            let object = fields.next().unwrap_or_default();
            let assume_unchanged = !tag.is_empty() && tag.chars().all(|c| c.is_ascii_lowercase());
            // A sparse checkout marks absent files skip-worktree. Only a present one can differ.
            let skip_worktree = tag.eq_ignore_ascii_case("s")
                && (!sparse || std::fs::symlink_metadata(toplevel.join(path)).is_ok());
            let hidden = assume_unchanged || skip_worktree;
            // A hidden link or submodule can change its target without git seeing it. A link
            // that still points where the index says is unchanged. Only hidden entries need
            // the comparison, which starts a git process.
            let link_now = hidden
                && std::fs::symlink_metadata(toplevel.join(path))
                    .is_ok_and(|metadata| metadata.is_symlink());
            let same_link = hidden
                && mode == "120000"
                && link_now
                && std::fs::read_link(toplevel.join(path)).is_ok_and(|target| {
                    git(toplevel.as_path(), &["cat-file", "blob", object])
                        .is_ok_and(|stored| stored == target.to_string_lossy())
                });
            if hidden && !same_link && (mode == "120000" || mode == "160000" || link_now) {
                hidden_links.push(path.to_string());
            }
            hidden.then(|| path.to_string())
        })
        .collect();
    // A submodule can hide edits from the outer diff: in its own index, through its own
    // `ignore` settings, or in nested submodules. Check each initialized one, recursively, and
    // collect its tracked links.
    let mut submodule_links = Vec::new();
    for entry in &index {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if meta.split(' ').nth(1) == Some("160000")
            && submodule_unknown(&toplevel, path, &mut submodule_links)
        {
            hidden_links.push(path.to_string());
        }
    }

    let (mut paths, mut special);
    // Untracked links: they work like tracked ones for the package that holds them.
    let mut untracked_links: Vec<String> = Vec::new();
    let mut uncertain = None;
    // A sparse checkout leaves tracked files out of the working tree: Cargo cannot see the
    // tests in them, whatever the selection.
    if sparse {
        let absent = index
            .iter()
            .filter_map(|entry| entry.split_once('\t'))
            .find(|(meta, path)| {
                meta.starts_with('S') && std::fs::symlink_metadata(toplevel.join(path)).is_err()
            });
        if let Some((_, path)) = absent {
            uncertain = Some(format!(
                "the sparse checkout leaves out tracked files such as `{path}`"
            ));
        }
    }
    if committed_only {
        (paths, special) = diff(&[&merge_base, "HEAD"])?;
        // The tool reads the workspace, its configuration and links from the working tree, so
        // the working tree must match HEAD: tracked, untracked and hidden files alike.
        let (dirty, _) = diff(&["HEAD"])?;
        let new_files = untracked()?;
        // A file removed from the index (`git rm --cached`) but on disk as in HEAD is the
        // same for Cargo.
        let unindexed: Vec<&String> = new_files
            .iter()
            .filter(|path| dirty.contains(path) && !differs(&toplevel, "HEAD", path))
            .collect();
        // Such a file can be a link that a package reads through: it still reaches its target.
        untracked_links.extend(
            unindexed
                .iter()
                .filter(|path| {
                    std::fs::symlink_metadata(toplevel.join(path))
                        .is_ok_and(|metadata| metadata.is_symlink())
                })
                .map(|path| path.to_string()),
        );
        let differing = dirty
            .iter()
            .chain(&new_files)
            .find(|path| !unindexed.contains(path))
            .or(hidden_links.first())
            .cloned()
            .or_else(|| {
                hidden
                    .iter()
                    .find(|path| differs(&toplevel, "HEAD", path))
                    .cloned()
            });
        if let Some(path) = differing {
            uncertain = Some(format!(
                "`{path}` differs from HEAD, and --committed needs a working tree that matches HEAD"
            ));
        }
    } else {
        (paths, special) = diff(&[&merge_base])?;
        let new_files = untracked()?;
        // A case-only rename on a case-insensitive file system: git can still read the old
        // spelling, so count both. On a case-sensitive one the two spellings are two files, and
        // git reports the deletion of a renamed one itself.
        let tracked: std::collections::BTreeMap<String, &str> = index
            .iter()
            .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path))
            .map(|path| (path.to_lowercase(), path))
            .collect();
        for path in &new_files {
            if let Some(old) = tracked
                .get(&path.to_lowercase())
                .filter(|old| **old != path)
                .filter(|old| same_entry(&toplevel.join(old), &toplevel.join(path)))
            {
                paths.push(old.to_string());
            }
        }
        for path in &new_files {
            // A path that ends with `/` is a nested repository: git does not list its files.
            let link = std::fs::symlink_metadata(toplevel.join(path)).is_ok_and(|m| m.is_symlink());
            if link {
                untracked_links.push(path.clone());
            }
            if link || path.ends_with('/') {
                special.push(path.trim_end_matches('/').to_string());
            }
        }
        // A file that a commit deleted and the working tree restores, untracked: a change
        // only when it differs from the base.
        let restored: Vec<String> = new_files
            .iter()
            .filter(|path| paths.contains(path) && !differs(&toplevel, &merge_base, path))
            .cloned()
            .collect();
        paths.retain(|path| !restored.contains(path));
        special.retain(|path| !restored.contains(path));
        paths.extend(
            new_files
                .into_iter()
                .filter(|path| !restored.contains(path)),
        );
        // Git compares a hidden entry by its index content, not by the file: the file
        // decides. A hidden link or submodule stays a change (below).
        let (changed, same): (Vec<String>, Vec<String>) = hidden
            .into_iter()
            .partition(|path| differs(&toplevel, &merge_base, path));
        let same: Vec<String> = same
            .into_iter()
            .filter(|path| !hidden_links.contains(path))
            .collect();
        paths.retain(|path| !same.contains(path));
        special.retain(|path| !same.contains(path));
        paths.extend(changed);
        // Hidden links and submodules with hidden edits are changes of unknown effect.
        paths.extend(hidden_links.iter().cloned());
        special.extend(hidden_links);
    }

    let root = normalize(workspace_root);
    let to_workspace = |paths: &[String]| -> Vec<String> {
        let mut out: Vec<String> = paths
            .iter()
            .map(|path| relative(&root, &toplevel.join(path)))
            .collect();
        out.sort();
        out.dedup();
        out
    };
    let files = to_workspace(&paths);
    let special = to_workspace(&special);

    let mut symlinks = Vec::new();
    // Cargo reads the working tree: a link there counts whatever type the index records,
    // for example after a staged replacement and a link restored in the working tree only.
    let tracked_links = index
        .iter()
        .filter_map(|entry| entry.split_once('\t'))
        .filter(|(meta, path)| {
            meta.split(' ').nth(1) == Some("120000")
                || std::fs::symlink_metadata(toplevel.join(path))
                    .is_ok_and(|metadata| metadata.is_symlink())
        })
        .map(|(_, path)| path.to_string());
    for path in tracked_links.chain(submodule_links).chain(untracked_links) {
        let link = toplevel.join(&path);
        let Ok(target) = std::fs::read_link(&link) else {
            continue;
        };
        let target = link.parent().unwrap_or(&toplevel).join(target);
        symlinks.push((relative(&root, &link), relative(&root, &normalize(&target))));
    }
    symlinks.sort();

    Ok(Changes {
        base,
        compared: merge_base,
        files,
        special,
        symlinks,
        repository: relative(&root, &toplevel),
        uncertain,
    })
}

/// The single merge base of `base` and `HEAD`. In a shallow clone, only a merge base that is one
/// of the two commits: the cut history can hide the true one.
fn merge_base_with(toplevel: &Path, base: &str) -> Result<String, String> {
    let merge_bases = git(toplevel, &["merge-base", "--all", base, "HEAD"]).map_err(|error| {
        format!("cannot find the merge base of `{base}` and HEAD (shallow clone?): {error}")
    })?;
    let merge_bases: Vec<&str> = merge_bases.split_whitespace().collect();
    let merge_base = match merge_bases.as_slice() {
        [one] => one.to_string(),
        _ => {
            return Err(format!(
                "`{base}` and HEAD have {} merge bases",
                merge_bases.len()
            ))
        }
    };
    // As for a pull request merge commit and HEAD^1.
    let shallow = git(toplevel, &["rev-parse", "--is-shallow-repository"])
        .is_ok_and(|value| value.trim() == "true");
    if shallow {
        let commit = |revision: &str| {
            git(
                toplevel,
                &[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("{revision}^{{commit}}"),
                ],
            )
            .map(|sha| sha.trim().to_string())
        };
        let ends = [commit(base), commit("HEAD")];
        if !ends
            .iter()
            .any(|end| end.as_deref() == Ok(merge_base.as_str()))
        {
            return Err(format!(
                "the clone is shallow and the merge base of `{base}` and HEAD is neither of them"
            ));
        }
    }
    Ok(merge_base)
}

/// True when two paths name one directory entry, as two spellings do on a case-insensitive
/// file system. False when the first does not exist. True when it cannot be told.
fn same_entry(first: &Path, second: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (
            std::fs::symlink_metadata(first),
            std::fs::symlink_metadata(second),
        ) {
            (Ok(first), Ok(second)) => first.dev() == second.dev() && first.ino() == second.ino(),
            (Err(_), _) => false,
            _ => true,
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::symlink_metadata(first).is_ok()
    }
}

/// True when an initialized submodule, or one nested in it, has changes or files that git
/// hides, or when its directory cannot be read. An uninitialized one is environment: false.
/// `path` is relative to `toplevel`; the submodule's tracked links are added to `links`, with
/// paths relative to `toplevel`.
fn submodule_unknown(toplevel: &Path, path: &str, links: &mut Vec<String>) -> bool {
    let dir = toplevel.join(path);
    // `exists` reads a permission error as "absent": the submodule would look uninitialized.
    match dir.join(".git").try_exists() {
        Ok(true) => {}
        Ok(false) => return false,
        Err(_) => return true,
    }
    let dir = dir.as_path();
    let status = git(
        dir,
        &["status", "--porcelain", "-z", "--ignore-submodules=none"],
    );
    let index = git(dir, &["ls-files", "-z", "-v", "-s"]);
    let (Ok(status), Ok(index)) = (status, index) else {
        return true;
    };
    if !status.is_empty() {
        return true;
    }
    let mut unknown = false;
    for entry in index.split('\0').filter(|entry| !entry.is_empty()) {
        let Some((meta, inner)) = entry.split_once('\t') else {
            return true;
        };
        let mut fields = meta.split(' ');
        let tag = fields.next().unwrap_or_default();
        let mode = fields.next().unwrap_or_default();
        let inner = format!("{path}/{inner}");
        unknown |= tag.chars().all(|c| c.is_ascii_lowercase()) || tag == "S";
        match mode {
            "120000" => links.push(inner),
            "160000" => unknown |= submodule_unknown(toplevel, &inner, links),
            _ => {}
        }
    }
    unknown
}

/// For a file that git hides (assume-unchanged or skip-worktree): true unless its working-tree
/// content and executable bit match `revision`. Any doubt counts as a difference.
fn differs(toplevel: &Path, revision: &str, path: &str) -> bool {
    let file = toplevel.join(path);
    let Ok(metadata) = std::fs::symlink_metadata(&file) else {
        return true; // deleted
    };
    // "<mode> <type> <object>\t<path>"
    let Ok(entry) = git(toplevel, &["ls-tree", revision, "--", path]) else {
        return true;
    };
    let mut fields = entry.split_whitespace();
    let (Some(mode), Some(_), Some(object)) = (fields.next(), fields.next(), fields.next()) else {
        return true; // not in that revision
    };
    // A file that became a link, or a link that became a file: hashing would follow the link.
    if metadata.is_symlink() != (mode == "120000") || mode == "160000" {
        return true;
    }
    // A link: git stores its target as the blob.
    if mode == "120000" {
        let stored = git(toplevel, &["cat-file", "blob", object]);
        let target = std::fs::read_link(&file);
        return match (stored, target) {
            (Ok(stored), Ok(target)) => stored != target.to_string_lossy(),
            _ => true,
        };
    }
    let Ok(hash) = git(toplevel, &["hash-object", "--path", path, "--", path]) else {
        return true;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Git keeps only the owner's execute bit.
        let executable = metadata.permissions().mode() & 0o100 != 0;
        if executable != (mode == "100755") {
            return true;
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, mode);
    hash.trim() != object
}

/// The branch that `origin/HEAD` points to. No guess when it is not set.
fn default_base(toplevel: &Path) -> Option<String> {
    let head = git(
        toplevel,
        // The full name: a short one can be ambiguous with a tag.
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .ok()?;
    let head = head.trim();
    (!head.is_empty()).then(|| head.to_string())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    for var in local_env_vars() {
        command.env_remove(var);
    }
    let output = command
        .arg("-C")
        .arg(dir)
        // A file system monitor hook can run any program and can report stale results.
        // A change of the executable bit can change a test, so always compare it. On a
        // case-insensitive file system, a case-only rename shows only with ignoreCase off.
        .args(["-c", "core.fsmonitor=false", "-c", "core.fileMode=true"])
        .args(["-c", "core.ignoreCase=false"])
        // Weaker stat checks can hide an edit that keeps the size and modification time.
        .args(["-c", "core.trustctime=true", "-c", "core.checkStat=default"])
        // A file that only looks modified (stat data) but matches the index is not a change.
        .args(["-c", "diff.autoRefreshIndex=true"])
        // Untracked files make a submodule changed; submodule status must not hide them.
        .args(["-c", "status.showUntrackedFiles=normal"])
        // An ambiguous revision name must warn, so that it counts as unknown.
        .args(["-c", "core.warnAmbiguousRefs=true"])
        .args(args)
        // A partial clone must not fetch missing objects: the command fails instead.
        // GIT_NO_LAZY_FETCH needs git 2.45; denying every transport covers older versions.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "none")
        // Selection only reads: no index refresh written as a side effect.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    // Git skips a directory that it cannot read with only a warning. The untracked files are
    // then unknown.
    // A merge base or a revision with a warning (an old reflog date, say) is not the asked one.
    let scans = args.contains(&"--others")
        || args.first() == Some(&"status")
        || args.first() == Some(&"merge-base")
        || args.first() == Some(&"rev-parse") && args.contains(&"--verify");
    // An ambiguous name (a branch and a tag) only warns: the revision is then unknown.
    let ambiguous = String::from_utf8_lossy(&output.stderr).contains("ambiguous");
    if output.status.success() && (scans && !output.stderr.is_empty() || ambiguous) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| "git printed a non-UTF-8 path".to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
