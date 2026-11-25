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
/// Without `base`, the base is the default branch of `origin`. Unless `committed_only`
/// is set, the list includes staged, unstaged and untracked (not ignored) files.
pub fn changed_files(
    workspace_root: &Path,
    base: Option<&str>,
    committed_only: bool,
) -> Result<Changes, Unknown> {
    let unknown = |base: Option<&str>, reason: String| Unknown {
        base: base.map(str::to_string),
        reason,
    };
    let toplevel = git(workspace_root, &["rev-parse", "--show-toplevel"])
        .map_err(|error| unknown(base, format!("not in a git work tree: {error}")))?;
    let toplevel = normalize(Path::new(toplevel.trim_end_matches(['\n', '\r'])));
    let base = match base {
        Some(base) => base.to_string(),
        None => default_base(&toplevel).ok_or_else(|| {
            unknown(
                None,
                "cannot find the default branch of `origin`; pass --base".to_string(),
            )
        })?,
    };
    let merge_base = git(&toplevel, &["merge-base", &base, "HEAD"]).map_err(|error| {
        unknown(
            Some(&base),
            format!("cannot find the merge base of `{base}` and HEAD (shallow clone?): {error}"),
        )
    })?;
    let merge_base = merge_base.trim().to_string();

    let diff = [
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--no-color",
    ];
    let mut listed = Vec::new();
    if committed_only {
        let mut args = diff.to_vec();
        args.extend([merge_base.as_str(), "HEAD", "--"]);
        listed.push(git(&toplevel, &args));
    } else {
        let mut args = diff.to_vec();
        args.extend([merge_base.as_str(), "--"]);
        listed.push(git(&toplevel, &args));
        listed.push(git(
            &toplevel,
            &["ls-files", "-z", "--others", "--exclude-standard"],
        ));
    }

    let root = normalize(workspace_root);
    let mut files = Vec::new();
    for output in listed {
        let output = output
            .map_err(|error| unknown(Some(&base), format!("cannot list changed files: {error}")))?;
        for path in output.split('\0').filter(|path| !path.is_empty()) {
            files.push(relative(&root, &toplevel.join(path)));
        }
    }
    files.sort();
    files.dedup();
    Ok(Changes {
        base,
        merge_base,
        files,
    })
}

/// `origin/HEAD` when the clone records it, else the first of `origin/main` and `origin/master`.
fn default_base(toplevel: &Path) -> Option<String> {
    if let Ok(head) = git(
        toplevel,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        let head = head.trim();
        if !head.is_empty() {
            return Some(head.to_string());
        }
    }
    ["origin/main", "origin/master"]
        .into_iter()
        .find(|candidate| {
            git(
                toplevel,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{candidate}^{{commit}}"),
                ],
            )
            .is_ok()
        })
        .map(str::to_string)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|_| "git printed a non-UTF-8 path".to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
