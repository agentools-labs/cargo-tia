//! Changed files from git. Reads the local repository only.

use crate::workspace::{normalize, relative};
use std::path::Path;
use std::process::Command;

/// The files that differ from the merge base, relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// The base revision as given or detected, for example `origin/main`.
    pub base: String,
    /// The merge base of `base` and `HEAD`.
    pub merge_base: String,
    /// Changed, added, deleted and (unless committed-only) untracked files.
    pub files: Vec<String>,
    /// Changed files that are or were a symbolic link or a submodule.
    pub special: Vec<String>,
    /// Tracked symbolic links as (link, resolved target), relative to the workspace root.
    pub symlinks: Vec<(String, String)>,
    /// The repository root relative to the workspace root: empty, or `..` segments.
    pub repository: String,
}

/// Why the change set is unknown. The caller then selects the full workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown {
    /// The base revision, when one was given or detected.
    pub base: Option<String>,
    pub reason: String,
}

/// Lists the files that changed since the merge base of `base` and `HEAD`.
///
/// Without `base`, the base is `origin/HEAD`, the default branch of `origin`. Unless
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
    let merge_bases = git(&toplevel, &["merge-base", "--all", &base, "HEAD"]).map_err(|error| {
        fail(format!(
            "cannot find the merge base of `{base}` and HEAD (shallow clone?): {error}"
        ))
    })?;
    let merge_bases: Vec<&str> = merge_bases.split_whitespace().collect();
    let merge_base = match merge_bases.as_slice() {
        [one] => one.to_string(),
        _ => {
            return Err(fail(format!(
                "`{base}` and HEAD have {} merge bases",
                merge_bases.len()
            )))
        }
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
            let assume_unchanged = !tag.is_empty() && tag.chars().all(|c| c.is_ascii_lowercase());
            // A sparse checkout marks absent files skip-worktree. Only a present one can differ.
            let skip_worktree = tag.eq_ignore_ascii_case("s")
                && (!sparse || std::fs::symlink_metadata(toplevel.join(path)).is_ok());
            let hidden = assume_unchanged || skip_worktree;
            // A hidden link or submodule can change its target without git seeing it.
            if hidden && (mode == "120000" || mode == "160000") {
                hidden_links.push(path.to_string());
            }
            hidden.then(|| path.to_string())
        })
        .collect();
    let is_manifest = |path: &String| path.rsplit('/').next() == Some("Cargo.toml");

    let (mut paths, mut special);
    if committed_only {
        (paths, special) = diff(&[&merge_base, "HEAD"])?;
        // The workspace graph comes from the working tree, so it must match HEAD.
        let (mut dirty, _) = diff(&["HEAD"])?;
        dirty.extend(untracked()?);
        dirty.extend(hidden.iter().cloned());
        if let Some(manifest) = dirty.iter().find(|path| is_manifest(path)) {
            return Err(fail(format!(
                "`{manifest}` may have uncommitted changes, so the workspace may differ from HEAD"
            )));
        }
    } else {
        (paths, special) = diff(&[&merge_base])?;
        let new_files = untracked()?;
        for path in &new_files {
            // A path that ends with `/` is a nested repository: git does not list its files.
            let link = std::fs::symlink_metadata(toplevel.join(path)).is_ok_and(|m| m.is_symlink());
            if link || path.ends_with('/') {
                special.push(path.trim_end_matches('/').to_string());
            }
        }
        paths.extend(new_files);
        paths.extend(hidden);
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
    for entry in &index {
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if meta.split(' ').nth(1) != Some("120000") {
            continue;
        }
        let link = toplevel.join(path);
        let Ok(target) = std::fs::read_link(&link) else {
            continue;
        };
        let target = link.parent().unwrap_or(&toplevel).join(target);
        symlinks.push((relative(&root, &link), relative(&root, &normalize(&target))));
    }
    symlinks.sort();

    Ok(Changes {
        base,
        merge_base,
        files,
        special,
        symlinks,
        repository: relative(&root, &toplevel),
    })
}

/// The branch that `origin/HEAD` points to. No guess when it is not set.
fn default_base(toplevel: &Path) -> Option<String> {
    let head = git(
        toplevel,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()?;
    let head = head.trim();
    (!head.is_empty()).then(|| head.to_string())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        // A file system monitor hook can run any program and can report stale results.
        // A change of the executable bit can change a test, so always compare it.
        .args(["-c", "core.fsmonitor=false", "-c", "core.fileMode=true"])
        .args(args)
        // A partial clone must not fetch missing objects: the command fails instead.
        // GIT_NO_LAZY_FETCH needs git 2.45; denying every transport covers older versions.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "none")
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    // Git skips a directory that it cannot read with only a warning. The untracked files are
    // then unknown.
    if output.status.success() && args.contains(&"--others") && !output.stderr.is_empty() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| "git printed a non-UTF-8 path".to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
