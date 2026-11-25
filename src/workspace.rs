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
    /// The package has a registry or git dependency.
    pub external: bool,
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
    /// Files that the manifests of non-member path dependencies name, such as `[lib] path`.
    pub foreign_files: Vec<String>,
    /// Directories of such files outside the dependency's own directory, with that directory.
    pub foreign_targets: Vec<(String, String)>,
    /// Source replacement directories (`directory` and `local-registry` in Cargo
    /// configuration). Registry packages from there can reach any member.
    pub source_dirs: Vec<String>,
    /// Cargo configuration files that the tool read, at the path Cargo uses and at the file it
    /// resolves to. A change to one selects the full workspace.
    pub config_files: Vec<String>,
    /// Included configuration files, each with the configuration file that includes it.
    pub config_includes: Vec<(String, String)>,
    /// Files behind member manifests that are symbolic links.
    pub manifest_targets: Vec<String>,
    /// Non-member path dependencies whose inherited dependencies cannot be found.
    pub unresolved: Vec<String>,
    /// Build scripts of the members, relative to the workspace root.
    pub build_scripts: Vec<String>,
    /// Target source files of the members, with the index of the member that declares each.
    pub targets: Vec<(String, usize)>,
    /// Target names by kind (`test`, `bin`, `example`, `bench`), with the declaring member.
    pub target_names: Vec<(String, String, usize)>,
    /// Directories that hold a target of a member that another package owns, with the
    /// declaring member's name.
    pub cross_targets: Vec<(String, String)>,
    /// Members that a `[patch]` or `[replace]` entry points to. Registry packages can then
    /// depend on them, and no-deps metadata does not show those edges. A patch path that is
    /// not a member counts as every member, because that package can depend on any member.
    pub patched: Vec<usize>,
    /// `full-run` globs from `[workspace.metadata.tia]`.
    pub full_run: Vec<String>,
    /// `ignore` globs from `[workspace.metadata.tia]`.
    pub ignore: Vec<String>,
}

/// Runs `cargo metadata --no-deps --offline` and builds the workspace model.
pub fn load(cargo: &OsStr, manifest_path: Option<&Path>) -> Result<Workspace, String> {
    let mut command = Command::new(cargo);
    // Through the rustup proxy, a missing toolchain must not be downloaded.
    command.env("RUSTUP_AUTO_INSTALL", "0");
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
    let mut workspace = from_metadata(&metadata)?;
    let Overrides {
        patched,
        others,
        sources,
        config_files,
        includes,
    } = overrides(&workspace);
    workspace.source_dirs = sources;
    workspace.config_files = config_files;
    workspace.config_includes = includes;
    // A local patch that is not a member is a path dependency of every member that has a
    // registry or git dependency: one of those can resolve to it. It can also depend on any
    // member, so every member counts as patched.
    if others.is_empty() {
        workspace.patched = patched;
    } else {
        workspace.patched = (0..workspace.packages.len()).collect();
        // A non-member path dependency can use a registry package that resolves to the
        // patch, so its users count too.
        let mut users: Vec<usize> = (0..workspace.packages.len())
            .filter(|&index| workspace.packages[index].external)
            .chain(
                workspace
                    .foreign
                    .iter()
                    .flat_map(|(_, users)| users.iter().copied()),
            )
            .collect();
        users.sort_unstable();
        users.dedup();
        let mut foreign: BTreeMap<String, Vec<usize>> = workspace.foreign.drain(..).collect();
        for dir in others {
            let entry = foreign.entry(dir).or_default();
            entry.extend(&users);
            entry.sort_unstable();
            entry.dedup();
        }
        workspace.foreign = foreign.into_iter().collect();
    }
    follow_path_dependencies(&mut workspace);
    add_nested_users(&mut workspace);
    mark_cross_targets(&mut workspace);
    // A workspace manifest that is a symbolic link: its target is a manifest too.
    let manifest = workspace.root.join("Cargo.toml");
    let target = normalize(&manifest);
    if target != manifest {
        let target = relative(&workspace.root, &target);
        workspace.manifest_targets.push(target);
    }
    Ok(workspace)
}

/// A non-member path dependency inside the source directory of a member's target can hold
/// modules of that target, so the member counts as one of its users.
fn add_nested_users(workspace: &mut Workspace) {
    for (src, declarer) in &workspace.targets {
        let source_dir = src.rsplit_once('/').map_or("", |(dir, _)| dir);
        for (dir, users) in &mut workspace.foreign {
            if contains(source_dir, dir) && !users.contains(declarer) {
                users.push(*declarer);
                users.sort_unstable();
            }
        }
    }
}

/// Who owns a path: the deepest member, or non-member path dependency, whose directory holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Member(usize),
    /// An index into `Workspace::foreign`.
    PathDependency(usize),
}

impl Workspace {
    /// The owner of a path relative to the workspace root. A path dependency wins over a
    /// member only when its directory is deeper.
    pub fn owner(&self, path: &str) -> Option<Owner> {
        let member = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| contains(&package.dir, path))
            .max_by_key(|(_, package)| depth(&package.dir));
        let foreign = self
            .foreign
            .iter()
            .enumerate()
            .filter(|(_, (dir, _))| contains(dir, path))
            .max_by_key(|(_, (dir, _))| depth(dir));
        if let Some((index, (dir, _))) = foreign {
            if member.is_none_or(|(_, package)| depth(dir) > depth(&package.dir)) {
                return Some(Owner::PathDependency(index));
            }
        }
        member.map(|(index, _)| Owner::Member(index))
    }
}

/// Records the directories of targets that the declaring member does not own, by the same
/// rule that maps changed files. A change there must reach the declaring member too.
fn mark_cross_targets(workspace: &mut Workspace) {
    let mut cross = workspace.foreign_targets.clone();
    for (src, declarer) in &workspace.targets {
        if workspace.owner(src) != Some(Owner::Member(*declarer)) {
            let parent = src.rsplit_once('/').map_or("", |(parent, _)| parent);
            cross.push((
                parent.to_string(),
                workspace.packages[*declarer].name.clone(),
            ));
        }
    }
    cross.sort();
    cross.dedup();
    workspace.cross_targets = cross;
}

/// Adds the path dependencies of non-member path dependencies, found by reading their
/// manifests as text, and their custom build scripts. No-deps metadata lists neither.
fn follow_path_dependencies(workspace: &mut Workspace) {
    let root = workspace.root.clone();
    let mut found: BTreeMap<String, Vec<usize>> = workspace.foreign.iter().cloned().collect();
    let mut queue = workspace.foreign.clone();
    while let Some((dir, users)) = queue.pop() {
        let base = root.join(&dir);
        let Ok(text) = std::fs::read_to_string(base.join("Cargo.toml")) else {
            continue;
        };
        for script in key_values(&text, "build") {
            let script = relative(&root, &normalize(&base.join(script)));
            workspace.build_scripts.push(script);
        }
        let mut paths: Vec<PathBuf> = key_values(&text, "path")
            .into_iter()
            .map(|path| base.join(path))
            .collect();
        // `workspace = true` takes a dependency from the crate's workspace manifest.
        if inherits(&text) && !is_workspace_root(&text) {
            // `package.workspace` names the workspace; otherwise it is the nearest ancestor.
            let named = key_values(&text, "workspace").into_iter().next();
            let manifest = match named {
                Some(named) => {
                    let dir = normalize(&base.join(named));
                    std::fs::read_to_string(dir.join("Cargo.toml"))
                        .ok()
                        .filter(|text| is_workspace_root(text))
                        .map(|text| (dir, text))
                }
                None => base.ancestors().skip(1).find_map(|dir| {
                    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
                    is_workspace_root(&text).then(|| (dir.to_path_buf(), text))
                }),
            };
            match manifest {
                Some((dir, text)) => {
                    // Edits to that manifest are invisible when it is outside the repository.
                    workspace
                        .foreign_files
                        .push(relative(&root, &dir.join("Cargo.toml")));
                    paths.extend(
                        key_values(&text, "path")
                            .into_iter()
                            .map(|path| dir.join(path)),
                    );
                }
                None => workspace.unresolved.push(dir.clone()),
            }
        }
        for path in paths {
            let target = normalize(&path);
            if !target.is_dir() {
                // A target path such as `src/lib.rs`. Outside the dependency's own directory,
                // another package owns it, so a change there selects the full workspace.
                let file = relative(&root, &target);
                if !contains(&dir, &file) {
                    let parent = file.rsplit_once('/').map_or("", |(parent, _)| parent);
                    workspace
                        .foreign_targets
                        .push((parent.to_string(), dir.clone()));
                }
                workspace.foreign_files.push(file);
                continue;
            }
            let dir = relative(&root, &target);
            if workspace.packages.iter().any(|package| package.dir == dir) {
                continue; // a member: its users already depend on every member
            }
            // Two path dependencies can share one: merge their users, and visit it again
            // when its users grow.
            let is_new = !found.contains_key(&dir);
            let known = found.entry(dir.clone()).or_default();
            let before = known.len();
            for user in &users {
                if !known.contains(user) {
                    known.push(*user);
                }
            }
            if is_new || known.len() > before {
                known.sort_unstable();
                queue.push((dir, known.clone()));
            }
        }
    }
    workspace.foreign = found.into_iter().collect();
    workspace.build_scripts.sort();
    workspace.build_scripts.dedup();
}

/// True when a manifest inherits a value from its workspace (`workspace = true`).
fn inherits(text: &str) -> bool {
    text.lines().any(|line| {
        let line: String = line
            .split('#')
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        line.contains("workspace=true")
    })
}

/// True when a manifest has a `[workspace]` table.
fn is_workspace_root(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "[workspace]")
}

/// What the workspace manifest and Cargo configuration redirect to local directories.
struct Overrides {
    /// Members that a `[patch]`, `[replace]` or `paths` entry points to.
    patched: Vec<usize>,
    /// Other local directories that such an entry points to.
    others: Vec<String>,
    /// Source replacement directories.
    sources: Vec<String>,
    /// Configuration files read, at both paths.
    config_files: Vec<String>,
    /// Included configuration files, with the file that includes each.
    includes: Vec<(String, String)>,
}

/// Reads the workspace manifest and the Cargo configuration files that Cargo reads for this
/// workspace, with the files they include.
fn overrides(workspace: &Workspace) -> Overrides {
    let root = &workspace.root;
    // Each file with the directory that its relative paths start from.
    let mut files = vec![(root.join("Cargo.toml"), root.clone())];
    // Cargo reads configuration from the directory it runs in and its parents, not from the
    // workspace root (they differ with --manifest-path).
    let cwd = std::env::current_dir().map(|cwd| normalize(&cwd)).ok();
    for dir in cwd.as_deref().unwrap_or(root).ancestors() {
        for name in [".cargo/config.toml", ".cargo/config"] {
            files.push((dir.join(name), dir.to_path_buf()));
        }
    }
    // Cargo resolves a relative CARGO_HOME from the directory it runs in.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .map(|home| match &cwd {
            Some(cwd) if home.is_relative() => lexical(&cwd.join(home)),
            _ => home,
        })
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(home) = cargo_home {
        let base = home.parent().unwrap_or(&home).to_path_buf();
        for name in ["config.toml", "config"] {
            files.push((home.join(name), base.clone()));
        }
    }
    let (mut patched, mut others, mut sources) = (Vec::new(), Vec::new(), Vec::new());
    let (mut config_files, mut includes) = (Vec::new(), Vec::new());
    let mut seen = std::collections::BTreeSet::new();
    while let Some((file, base)) = files.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        // Record the path before reading it: deleting a configuration file is a change too.
        if !file.ends_with("Cargo.toml") {
            config_files.push(relative(root, &file));
            config_files.push(relative(root, &normalize(&file)));
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let paths = patch_paths(&text);
        let mut recursive = Vec::new();
        if !file.ends_with("Cargo.toml") {
            // Cargo finds packages anywhere below a `paths` entry.
            recursive = key_strings(&text, "paths");
            let dir = file.parent().unwrap_or(root).to_path_buf();
            for include in key_strings(&text, "include") {
                // Cargo resolves an include, and the paths in it, from where the include
                // points, even when that is a symbolic link. Changes count at both places.
                let included = lexical(&dir.join(include));
                includes.push((relative(root, &included), relative(root, &file)));
                // Relative paths in a configuration file start from the parent of its directory.
                let included_base = included
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or(root)
                    .to_path_buf();
                files.push((included, included_base));
            }
            for key in ["directory", "local-registry"] {
                for source in key_strings(&text, key) {
                    sources.push(relative(root, &normalize(&base.join(source))));
                }
            }
        }
        for path in paths {
            let dir = relative(root, &normalize(&base.join(path)));
            match workspace.packages.iter().position(|p| p.dir == dir) {
                Some(index) => patched.push(index),
                None => others.push(dir),
            }
        }
        // Any crate below a `paths` override directory can replace a registry package, with
        // its own manifest and build script: a change there selects the full workspace.
        for path in recursive {
            sources.push(relative(root, &normalize(&base.join(path))));
        }
    }
    patched.sort_unstable();
    patched.dedup();
    others.sort();
    others.dedup();
    sources.sort();
    sources.dedup();
    config_files.sort();
    config_files.dedup();
    includes.sort();
    includes.dedup();
    Overrides {
        patched,
        others,
        sources,
        config_files,
        includes,
    }
}

/// The `path` values of entries in `[patch...]` and `[replace]` tables and dotted `patch.` keys.
/// This reads TOML as text: it skips comments, and an unusual layout can only add paths.
fn patch_paths(text: &str) -> Vec<String> {
    let mut in_table = false;
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or_default().trim();
        if line.starts_with('#') {
            continue;
        }
        let key = line.trim_start_matches('[').trim_start();
        let is_patch = ["patch", "replace"].iter().any(|name| {
            key.strip_prefix(name)
                .is_some_and(|rest| rest.trim_start().starts_with(['.', ']', '=']))
        });
        if line.starts_with('[') {
            in_table = is_patch;
        } else if in_table || is_patch {
            paths.extend(line_values(line, "path"));
        }
    }
    paths
}

/// The quoted strings of a top-level `key = "..."` or `key = [...]` (also over several lines)
/// in a Cargo configuration file, such as `paths`, `include` or `directory`.
fn key_strings(text: &str, key: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut in_array = false;
    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or_default().trim();
        if line.starts_with('#') {
            continue;
        }
        let rest = if in_array {
            line
        } else {
            let value = line
                .strip_prefix(key)
                .map(str::trim_start)
                .and_then(|after| after.strip_prefix('='))
                .map(str::trim_start);
            let Some(value) = value else {
                continue;
            };
            in_array = value.starts_with('[');
            value
        };
        let end = if in_array { rest.find(']') } else { None };
        let inside = end.map_or(rest, |end| &rest[..end]);
        for (index, part) in inside.split(['"', '\'']).enumerate() {
            if index % 2 == 1 {
                strings.push(part.to_string());
            }
        }
        if end.is_some() {
            in_array = false;
        }
    }
    strings
}

/// The quoted values of `key = ...` in all lines of a TOML text, skipping comment lines.
fn key_values(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split(" #").next().unwrap_or_default().trim())
        .filter(|line| !line.starts_with('#'))
        .flat_map(|line| line_values(line, key))
        .collect()
}

/// The quoted values of `key = ...` in one line of TOML.
fn line_values(line: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find(key) {
        let before = rest[..at].chars().next_back();
        let whole_key = !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-');
        rest = &rest[at + key.len()..];
        // A quoted key such as `"build" = ...`.
        if let Some(quote) = before.filter(|c| *c == '"' || *c == '\'') {
            rest = rest.strip_prefix(quote).unwrap_or(rest);
        }
        let Some(value) = rest.trim_start().strip_prefix('=').map(str::trim_start) else {
            continue;
        };
        let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        if let Some(end) = value[1..].find(quote) {
            if whole_key {
                values.push(value[1..1 + end].to_string());
            }
        }
    }
    values
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
        // A manifest that is a symbolic link: its target is a manifest too.
        let target = normalize(Path::new(manifest));
        if target != normalize(dir).join("Cargo.toml") {
            workspace.manifest_targets.push(relative(&root, &target));
        }
        workspace.packages.push(Package {
            name: name.to_string(),
            dir: relative(&root, &normalize(dir)),
            deps: Vec::new(),
            external: false,
        });
    }

    let mut foreign: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    // Source directories of targets other than build scripts, with the declaring package.
    let mut source_dirs: Vec<(String, usize)> = Vec::new();
    for (index, package) in raw.iter().enumerate() {
        let is_root = workspace.packages[index].dir.is_empty();
        for target in package["targets"].as_array().into_iter().flatten() {
            let name = target["name"].as_str().unwrap_or_default();
            for kind in target["kind"].as_array().into_iter().flatten() {
                if let Some(kind) = kind.as_str() {
                    let names = &mut workspace.target_names;
                    names.push((kind.to_string(), name.to_string(), index));
                }
            }
            let Some(src) = target["src_path"].as_str() else {
                continue;
            };
            // Rust finds the modules of a target beside the path that Cargo names, also when
            // that path is a symbolic link: keep it, and track the file behind it too.
            let declared = locate(Path::new(src));
            let linked = normalize(Path::new(src));
            let src = relative(&root, &declared);
            if linked != declared {
                workspace.targets.push((relative(&root, &linked), index));
            }
            let is_build_script = target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "custom-build"));
            if is_build_script {
                workspace.build_scripts.push(src.clone());
            }
            // Every target counts, build scripts too. A build script in the workspace root
            // makes its package depend on every member: at worst that selects too much.
            let dir = src.rsplit_once('/').map_or("", |(dir, _)| dir);
            source_dirs.push((dir.to_string(), index));
            if is_root {
                let entry = src.split('/').next().unwrap_or_default().to_string();
                if !entry.is_empty() && !workspace.root_entries.contains(&entry) {
                    workspace.root_entries.push(entry);
                }
            }
            workspace.targets.push((src, index));
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
                None => {
                    workspace.packages[index].external = true;
                    workspace.packages.iter().position(|p| p.name == dep_name)
                }
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
    // A member inside the source directory of another package's target can hold modules of
    // that target (`mod engine;` with a member at `src/engine/`). Count it as a dependency.
    let mut nested = Vec::new();
    for (source_dir, declarer) in &source_dirs {
        for (member, package) in workspace.packages.iter().enumerate() {
            if member != *declarer
                && package.dir != *source_dir
                && contains(source_dir, &package.dir)
            {
                nested.push((*declarer, member));
            }
        }
    }
    for (declarer, member) in nested {
        let deps = &mut workspace.packages[declarer].deps;
        if !deps.contains(&member) {
            deps.push(member);
            deps.sort_unstable();
        }
    }
    mark_cross_targets(&mut workspace);

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

/// How deep a directory lies below the workspace root: negative above it.
pub fn depth(dir: &str) -> i64 {
    dir.split('/')
        .filter(|part| !part.is_empty())
        .map(|part| if part == ".." { -1 } else { 1 })
        .sum()
}

/// True when `path` lies in the directory `dir`; both are relative to the workspace root, and
/// the empty `dir` is the root itself.
pub fn contains(dir: &str, path: &str) -> bool {
    let ups = |text: &str| text.split('/').take_while(|part| *part == "..").count();
    // The workspace root or a directory above it: it holds every path that does not climb
    // higher than it.
    if dir.split('/').all(|part| part.is_empty() || part == "..") {
        let depth = if dir.is_empty() { 0 } else { ups(dir) };
        return path != dir && ups(path) <= depth;
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Resolves symbolic links when the path exists; otherwise removes `.` and `..` lexically.
pub fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    // A deleted file: resolve the links of the directory that still holds it.
    let path = lexical(path);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
            Ok(parent) => parent.join(name),
            Err(_) => path,
        },
        _ => path,
    }
}

/// Resolves the links of the directories in a path but not of its last part.
pub fn locate(path: &Path) -> PathBuf {
    let path = lexical(path);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => normalize(parent).join(name),
        _ => normalize(&path),
    }
}

/// Removes `.` and `..` segments without reading the file system.
pub fn lexical(path: &Path) -> PathBuf {
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
    fn a_path_dependency_above_the_root_does_not_own_member_files() {
        let ws = Workspace {
            packages: vec![Package {
                name: "ui".to_string(),
                dir: "ui".to_string(),
                deps: Vec::new(),
                external: false,
            }],
            foreign: vec![("../..".to_string(), vec![0])],
            ..Workspace::default()
        };
        assert_eq!(ws.owner("ui/fixtures/case.json"), Some(Owner::Member(0)));
        assert_eq!(ws.owner("../other/x"), Some(Owner::PathDependency(0)));
        assert_eq!(depth("../.."), -2);
        assert_eq!(depth("ui/fixtures"), 2);
    }

    #[test]
    fn containment_of_the_root_and_its_parents() {
        assert!(contains("", "crates/a/x.rs"));
        assert!(!contains("", "../web/x"));
        assert!(contains("..", "crates/a/x.rs"));
        assert!(contains("..", "../web/x"));
        assert!(!contains("..", "../../other/x"));
        assert!(contains("../..", "../../other/x"));
        assert!(contains("../web", "../web/x"));
        assert!(!contains("../web", "../webx/y"));
        assert!(contains("crates", "crates/a"));
        assert!(!contains("crates", "crates-extra/a"));
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
    fn normalize_resolves_the_directory_of_a_missing_file() {
        let dir = std::env::temp_dir();
        let missing = dir.join("cargo-tia-no-such-file");
        assert_eq!(
            normalize(&missing),
            std::fs::canonicalize(&dir)
                .unwrap()
                .join("cargo-tia-no-such-file")
        );
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
        // app -> core through a registry-style name match; root -> app through its path, and
        // root -> every member because its build script sits in the workspace root.
        assert_eq!(ws.packages[0].deps, [1]);
        assert_eq!(ws.packages[1].deps, Vec::<usize>::new());
        assert_eq!(ws.packages[2].deps, [0, 1]);
        assert_eq!(ws.foreign, [("vendor/vendored".to_string(), vec![1])]);
        assert_eq!(ws.build_scripts, ["build.rs", "crates/app/gen.rs"]);
        assert_eq!(ws.root_entries, ["build.rs", "src", "tests"]);
        assert_eq!(ws.full_run, ["ci/**"]);
        assert_eq!(ws.ignore, ["**/*.md"]);
        // app has registry dependencies; the others have path dependencies only.
        assert!(ws.packages[0].external && !ws.packages[1].external && !ws.packages[2].external);
        assert!(ws.cross_targets.is_empty());
    }

    #[test]
    fn targets_outside_the_package_directory_are_recorded() {
        let metadata = json!({
            "workspace_root": "/nonexistent/ws",
            "workspace_members": ["path+file:///nonexistent/ws/app#app@0.1.0"],
            "packages": [package("app", "/nonexistent/ws/app", vec![], vec![
                target("lib", "/nonexistent/ws/app/src/lib.rs"),
                target("test", "/nonexistent/ws/fixtures/src/check.rs"),
            ])],
        });
        let ws = from_metadata(&metadata).unwrap();
        assert_eq!(
            ws.cross_targets,
            [("fixtures/src".to_string(), "app".to_string())]
        );
    }

    #[test]
    fn root_package_targets_under_a_member_are_recorded() {
        let metadata = json!({
            "workspace_root": "/nonexistent/ws",
            "workspace_members": [
                "path+file:///nonexistent/ws#root@0.1.0",
                "path+file:///nonexistent/ws/crates/helper#helper@0.1.0",
            ],
            "packages": [
                package("root", "/nonexistent/ws", vec![], vec![
                    target("bin", "/nonexistent/ws/src/main.rs"),
                    target("test", "/nonexistent/ws/crates/helper/tests/shared.rs"),
                ]),
                package("helper", "/nonexistent/ws/crates/helper", vec![], vec![
                    target("lib", "/nonexistent/ws/crates/helper/src/lib.rs"),
                ]),
            ],
        });
        let ws = from_metadata(&metadata).unwrap();
        assert_eq!(
            ws.cross_targets,
            [("crates/helper/tests".to_string(), "root".to_string())]
        );
    }

    #[test]
    fn reads_key_strings() {
        let override_paths = |text| key_strings(text, "paths");
        assert_eq!(
            key_strings("include = \"more.toml\"\npaths = []", "include"),
            ["more.toml"]
        );
        assert_eq!(
            key_strings(
                "include = [{ path = \"a.toml\", optional = true }]",
                "include"
            ),
            ["a.toml"]
        );
        assert_eq!(
            key_strings("[source.v]\ndirectory = \"vendor\"", "directory"),
            ["vendor"]
        );
        assert_eq!(
            override_paths("paths = [\"../a\", '../b']"),
            ["../a", "../b"]
        );
        assert_eq!(
            override_paths("[build]\njobs = 2\n\npaths = [\n  \"../c\", # one\n  \"../d\",\n]\n"),
            ["../c", "../d"]
        );
        assert!(override_paths("# paths = [\"../e\"]\nsearch-paths = 1").is_empty());
    }

    #[test]
    fn members_in_another_package_source_directory_become_dependencies() {
        let metadata = json!({
            "workspace_root": "/nonexistent/ws",
            "workspace_members": [
                "path+file:///nonexistent/ws/app#app@0.1.0",
                "path+file:///nonexistent/ws/app/src/engine#engine@0.1.0",
            ],
            "packages": [
                package("app", "/nonexistent/ws/app", vec![], vec![
                    target("lib", "/nonexistent/ws/app/src/lib.rs"),
                ]),
                package("engine", "/nonexistent/ws/app/src/engine", vec![], vec![
                    target("lib", "/nonexistent/ws/app/src/engine/src/lib.rs"),
                ]),
            ],
        });
        let ws = from_metadata(&metadata).unwrap();
        // app (index 0) now depends on engine (index 1).
        assert_eq!(ws.packages[0].deps, [1]);
        assert!(ws.packages[1].deps.is_empty());
    }

    #[test]
    fn path_dependencies_inside_a_source_directory_gain_its_package_as_user() {
        let mut ws = Workspace {
            packages: vec![
                Package {
                    name: "app".to_string(),
                    dir: "crates/app".to_string(),
                    deps: Vec::new(),
                    external: false,
                },
                Package {
                    name: "lonely".to_string(),
                    dir: "crates/lonely".to_string(),
                    deps: Vec::new(),
                    external: false,
                },
            ],
            targets: vec![
                ("crates/app/src/lib.rs".to_string(), 0),
                ("crates/app/build.rs".to_string(), 0),
            ],
            build_scripts: vec!["crates/app/build.rs".to_string()],
            foreign: vec![
                ("crates/app/src/engine".to_string(), vec![1]),
                ("crates/app/tools".to_string(), vec![1]),
            ],
            ..Workspace::default()
        };
        add_nested_users(&mut ws);
        assert_eq!(ws.foreign[0].1, [0, 1]);
        // The build script's directory counts too.
        assert_eq!(ws.foreign[1].1, [0, 1]);
    }

    #[test]
    fn finds_inherited_values() {
        assert!(inherits("[dependencies]\nfoo.workspace = true"));
        assert!(inherits("foo = { workspace = true, features = [\"x\"] }"));
        assert!(!inherits("# foo.workspace = true\nbar = \"1\""));
        assert!(is_workspace_root(
            "[package]\nname = \"x\"\n\n[workspace]\n"
        ));
        assert!(!is_workspace_root("[workspace.dependencies]\n"));
    }

    #[test]
    fn reads_key_values() {
        let text = "[package]\nbuild = \"gen.rs\" # custom\n# build = \"old.rs\"\nbuilder = \"x\"\n[build-dependencies]\nb = { path = '../b' }\n";
        assert_eq!(key_values(text, "build"), ["gen.rs"]);
        assert_eq!(key_values(text, "path"), ["../b"]);
        assert!(key_values("[package]\nbuild = false", "build").is_empty());
        assert_eq!(key_values("\"build\" = \"gen.rs\"", "build"), ["gen.rs"]);
        assert_eq!(key_values("b = { 'path' = \"../b\" }", "path"), ["../b"]);
    }

    #[test]
    fn reads_patch_paths() {
        let paths = |text: &str| patch_paths(text);
        assert_eq!(
            paths("[patch.crates-io]\nfoo = { path = \"x\" } # local"),
            ["x"]
        );
        assert_eq!(
            paths("[ patch.'crates-io'.foo ]\npath = 'y'\nversion = \"1\""),
            ["y"]
        );
        assert_eq!(paths("patch.crates-io.foo.path = \"z\""), ["z"]);
        assert_eq!(paths("[replace]\n\"foo:0.1.0\" = { path = \"w\" }"), ["w"]);
        // Commented entries, other tables, git sources and other keys hold no patch path.
        assert!(paths("[patch.'crates-io']\n# rowan = { path = \"../rowan\" }\n").is_empty());
        assert!(paths("[dependencies]\nfoo = { path = \"a\" }").is_empty());
        assert!(paths("[patch.crates-io]\nfoo = { git = \"https://example.com/foo\" }").is_empty());
        assert!(paths("[patch.crates-io]\nfoo = { filepath = \"q\" }").is_empty());
        assert!(
            paths("[patch.crates-io]\na = { path = \"x\" }\n[dependencies]\nb = { path = \"y\" }")
                == ["x"]
        );
        assert!(paths("[package]\nname = \"patchwork\"").is_empty());
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
