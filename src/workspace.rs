//! The workspace model, read from `cargo metadata --no-deps`.

use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// A workspace member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Directory of the manifest, relative to the workspace root; empty for the root package.
    pub dir: String,
    /// Indices of the workspace members this package depends on, any dependency kind.
    pub deps: Vec<usize>,
}

/// The parts of a workspace that decide which packages a change affects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Canonical absolute path of the workspace root.
    pub root: PathBuf,
    /// Workspace members, sorted by name.
    pub packages: Vec<Package>,
    /// For a package at the workspace root: the top-level entries that hold its targets
    /// (for example `src` and `tests`). Other root files belong to the workspace, not to it.
    pub root_entries: Vec<String>,
    /// Directories of path dependencies that are not members, with the members that use them.
    pub foreign: Vec<(String, Vec<usize>)>,
    /// Build scripts of the members, relative to the workspace root.
    pub build_scripts: Vec<String>,
    /// `full-run` globs from `[workspace.metadata.tia]`.
    pub full_run: Vec<String>,
    /// `ignore` globs from `[workspace.metadata.tia]`.
    pub ignore: Vec<String>,
}

/// Runs `cargo metadata --no-deps --offline` and builds the workspace model.
pub fn load(cargo: &OsStr, manifest_path: Option<&Path>) -> Result<Workspace, String> {
    let mut command = Command::new(cargo);
    command.args([
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--offline",
    ]);
    if let Some(path) = manifest_path {
        command.arg("--manifest-path").arg(path);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run `cargo metadata`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`cargo metadata` failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cannot parse `cargo metadata` output: {error}"))?;
    from_metadata(&metadata)
}

/// Builds the workspace model from `cargo metadata` JSON.
pub fn from_metadata(metadata: &Value) -> Result<Workspace, String> {
    let root = metadata["workspace_root"]
        .as_str()
        .ok_or("`cargo metadata` output has no workspace_root")?;
    let root = normalize(Path::new(root));
    let members: Vec<&str> = metadata["workspace_members"]
        .as_array()
        .ok_or("`cargo metadata` output has no workspace_members")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut raw: Vec<&Value> = metadata["packages"]
        .as_array()
        .ok_or("`cargo metadata` output has no packages")?
        .iter()
        .filter(|package| {
            package["id"]
                .as_str()
                .is_some_and(|id| members.contains(&id))
        })
        .collect();
    raw.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut workspace = Workspace {
        root: root.clone(),
        ..Workspace::default()
    };
    for package in &raw {
        let name = package["name"].as_str().ok_or("package without a name")?;
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or_else(|| format!("package `{name}` has no manifest_path"))?;
        let dir = Path::new(manifest).parent().unwrap_or(Path::new(manifest));
        workspace.packages.push(Package {
            name: name.to_string(),
            dir: relative(&root, &normalize(dir)),
            deps: Vec::new(),
        });
    }

    let mut foreign: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, package) in raw.iter().enumerate() {
        let is_root = workspace.packages[index].dir.is_empty();
        for target in package["targets"].as_array().into_iter().flatten() {
            let Some(src) = target["src_path"].as_str() else {
                continue;
            };
            let src = relative(&root, &normalize(Path::new(src)));
            let is_build_script = target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "custom-build"));
            if is_build_script {
                workspace.build_scripts.push(src.clone());
            }
            if is_root {
                let entry = src.split('/').next().unwrap_or_default().to_string();
                if !entry.is_empty() && !workspace.root_entries.contains(&entry) {
                    workspace.root_entries.push(entry);
                }
            }
        }
        let mut deps = Vec::new();
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let dep_name = dependency["name"].as_str().unwrap_or_default();
            let target = match dependency["path"].as_str() {
                Some(path) => {
                    let dir = relative(&root, &normalize(Path::new(path)));
                    match workspace.packages.iter().position(|p| p.dir == dir) {
                        Some(member) => Some(member),
                        None => {
                            let users = foreign.entry(dir).or_default();
                            if !users.contains(&index) {
                                users.push(index);
                            }
                            None
                        }
                    }
                }
                // A registry or git dependency named like a member may be redirected to the
                // member by `[patch]`, so count it as an edge. At worst this selects too much.
                None => workspace.packages.iter().position(|p| p.name == dep_name),
            };
            if let Some(member) = target {
                if member != index && !deps.contains(&member) {
                    deps.push(member);
                }
            }
        }
        deps.sort_unstable();
        workspace.packages[index].deps = deps;
    }
    workspace.foreign = foreign.into_iter().collect();
    workspace.build_scripts.sort();
    workspace.root_entries.sort();

    let config = &metadata["metadata"]["tia"];
    workspace.full_run = string_list(config, "full-run")?;
    workspace.ignore = string_list(config, "ignore")?;
    Ok(workspace)
}

fn string_list(config: &Value, key: &str) -> Result<Vec<String>, String> {
    match &config[key] {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    format!("`[workspace.metadata.tia] {key}` must be an array of strings")
                })
            })
            .collect(),
        _ => Err(format!(
            "`[workspace.metadata.tia] {key}` must be an array of strings"
        )),
    }
}

/// Resolves symbolic links when the path exists; otherwise removes `.` and `..` lexically.
pub fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The `/`-separated path of `path` relative to `root`, with `..` segments when it is outside.
pub fn relative(root: &Path, path: &Path) -> String {
    let root: Vec<Component> = root.components().collect();
    let path: Vec<Component> = path.components().collect();
    let common = root.iter().zip(&path).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); root.len() - common];
    parts.extend(
        path[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dep(name: &str, path: Option<&str>) -> Value {
        match path {
            Some(path) => json!({"name": name, "kind": null, "path": path}),
            None => {
                json!({"name": name, "kind": "dev", "source": "registry+https://github.com/rust-lang/crates.io-index"})
            }
        }
    }

    fn package(name: &str, dir: &str, deps: Vec<Value>, targets: Vec<Value>) -> Value {
        json!({
            "name": name,
            "id": format!("path+file://{dir}#{name}@0.1.0"),
            "manifest_path": format!("{dir}/Cargo.toml"),
            "dependencies": deps,
            "targets": targets,
        })
    }

    fn target(kind: &str, src: &str) -> Value {
        json!({"kind": [kind], "src_path": src})
    }

    #[test]
    fn relative_paths_inside_and_outside_the_root() {
        let root = Path::new("/w/ws");
        assert_eq!(relative(root, Path::new("/w/ws")), "");
        assert_eq!(relative(root, Path::new("/w/ws/a/b.rs")), "a/b.rs");
        assert_eq!(relative(root, Path::new("/w/shared/x")), "../shared/x");
        assert_eq!(relative(root, Path::new("/elsewhere")), "../../elsewhere");
    }

    #[test]
    fn normalize_removes_dot_segments_of_missing_paths() {
        assert_eq!(
            normalize(Path::new("/no/such/dir/a/../b/./c")),
            PathBuf::from("/no/such/dir/b/c")
        );
    }

    #[test]
    fn builds_members_edges_foreign_paths_and_config() {
        let metadata = json!({
            "workspace_root": "/nonexistent/ws",
            "workspace_members": [
                "path+file:///nonexistent/ws#root@0.1.0",
                "path+file:///nonexistent/ws/crates/core#core@0.1.0",
                "path+file:///nonexistent/ws/crates/app#app@0.1.0",
            ],
            "packages": [
                package("root", "/nonexistent/ws", vec![dep("app", Some("/nonexistent/ws/crates/app"))], vec![
                    target("bin", "/nonexistent/ws/src/main.rs"),
                    target("test", "/nonexistent/ws/tests/it.rs"),
                    target("custom-build", "/nonexistent/ws/build.rs"),
                ]),
                package("core", "/nonexistent/ws/crates/core", vec![
                    dep("vendored", Some("/nonexistent/ws/crates/core/../../vendor/vendored")),
                ], vec![target("lib", "/nonexistent/ws/crates/core/src/lib.rs")]),
                package("app", "/nonexistent/ws/crates/app", vec![
                    dep("core", None),
                    dep("serde", None),
                ], vec![target("custom-build", "/nonexistent/ws/crates/app/gen.rs")]),
            ],
            "metadata": {"tia": {"full-run": ["ci/**"], "ignore": ["**/*.md"]}},
        });
        let ws = from_metadata(&metadata).unwrap();
        let names: Vec<&str> = ws.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["app", "core", "root"]);
        assert_eq!(ws.packages[0].dir, "crates/app");
        assert_eq!(ws.packages[2].dir, "");
        // app -> core through a registry-style name match; root -> app through its path.
        assert_eq!(ws.packages[0].deps, [1]);
        assert_eq!(ws.packages[1].deps, Vec::<usize>::new());
        assert_eq!(ws.packages[2].deps, [0]);
        assert_eq!(ws.foreign, [("vendor/vendored".to_string(), vec![1])]);
        assert_eq!(ws.build_scripts, ["build.rs", "crates/app/gen.rs"]);
        assert_eq!(ws.root_entries, ["build.rs", "src", "tests"]);
        assert_eq!(ws.full_run, ["ci/**"]);
        assert_eq!(ws.ignore, ["**/*.md"]);
    }

    #[test]
    fn rejects_malformed_config() {
        let metadata = json!({
            "workspace_root": "/nonexistent/ws",
            "workspace_members": [],
            "packages": [],
            "metadata": {"tia": {"ignore": "docs/**"}},
        });
        let error = from_metadata(&metadata).unwrap_err();
        assert!(error.contains("ignore"), "{error}");
    }
}
