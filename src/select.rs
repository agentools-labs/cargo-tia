//! Maps changed files to packages and selects their reverse dependency closure.

use crate::glob;
use crate::workspace::{Owner, Workspace};
use std::collections::VecDeque;

/// Files whose change can affect every package. Globs are relative to the workspace root.
/// A member manifest is one too: it can change the features that Cargo unifies for other
/// packages.
pub const FULL_RUN: &[&str] = &[
    "**/Cargo.toml",
    "**/Cargo.lock",
    "**/rust-toolchain",
    "**/rust-toolchain.toml",
    "**/.cargo/config",
    "**/.cargo/config.toml",
    "**/build.rs",
];

/// What one changed file does to the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The file matches a full-run glob or is a build script.
    FullRun(String),
    /// The file matches an ignore glob.
    Ignored(String),
    /// The file lies in this package.
    Package(usize),
    /// The file lies in a path dependency outside the workspace, used by these packages.
    PathDependency(Vec<usize>),
    /// The file lies in no package: workspace files, scripts, documents.
    Outside,
}

/// Why a package is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The change set selects the whole workspace.
    Full,
    /// A file of the package, or of a path dependency it uses, changed.
    Changed,
    /// The package depends on this selected package.
    DependsOn(usize),
}

/// A changed file, or a path that reaches a changed file through a symbolic link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub effect: Effect,
    /// For a path through a link: the changed file that the link reaches.
    pub link_to: Option<String>,
}

/// The packages to test, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Why the selection is the full workspace; empty when it is not.
    pub full_reasons: Vec<String>,
    /// Each changed file and its effect.
    pub files: Vec<File>,
    /// Selected package indices with their reasons, sorted by index (that is, by name).
    pub packages: Vec<(usize, Reason)>,
}

impl Selection {
    pub fn is_full(&self) -> bool {
        !self.full_reasons.is_empty()
    }

    /// Makes this selection the full workspace for one more reason, and keeps the files.
    pub fn make_full(&mut self, workspace: &Workspace, reason: String) {
        self.full_reasons.push(reason);
        self.packages = (0..workspace.packages.len())
            .map(|index| (index, Reason::Full))
            .collect();
    }

    /// The full workspace, for a reason that is not a single file.
    pub fn full(workspace: &Workspace, reason: String) -> Self {
        Selection {
            full_reasons: vec![reason],
            files: Vec::new(),
            packages: (0..workspace.packages.len())
                .map(|index| (index, Reason::Full))
                .collect(),
        }
    }
}

/// Extra globs given on the command line, added to those of the workspace.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub full_run: Vec<String>,
    pub ignore: Vec<String>,
}

/// Decides what one changed file does. `path` is relative to the workspace root.
pub fn classify(workspace: &Workspace, rules: &Rules, path: &str) -> Effect {
    let full_run = FULL_RUN
        .iter()
        .copied()
        .chain(workspace.full_run.iter().map(String::as_str))
        .chain(rules.full_run.iter().map(String::as_str));
    for pattern in full_run {
        if glob::matches(pattern, path) {
            return Effect::FullRun(format!("matches full-run glob `{pattern}`"));
        }
    }
    if workspace.build_scripts.iter().any(|script| script == path) {
        return Effect::FullRun("is a build script".to_string());
    }
    if workspace.config_files.iter().any(|file| file == path) {
        return Effect::FullRun("is a Cargo configuration file".to_string());
    }
    if workspace.manifest_targets.iter().any(|file| file == path) {
        return Effect::FullRun("is a package manifest behind a symbolic link".to_string());
    }
    if workspace
        .source_dirs
        .iter()
        .any(|dir| crate::workspace::contains(dir, path))
    {
        return Effect::FullRun("is in a source replacement directory".to_string());
    }
    if let Some((_, package)) = workspace
        .cross_targets
        .iter()
        .find(|(dir, _)| crate::workspace::contains(dir, path))
    {
        return Effect::FullRun(format!(
            "is next to a target of `{package}` outside that package"
        ));
    }
    let ignore = workspace.ignore.iter().chain(&rules.ignore);
    for pattern in ignore {
        if glob::matches(pattern, path) {
            return Effect::Ignored(pattern.clone());
        }
    }

    match workspace.owner(path) {
        None => Effect::Outside,
        // Path dependencies can nest (a `paths` override directory holds crates that members
        // also use directly): the users of every one that holds the file count.
        Some(Owner::PathDependency(_)) => {
            let mut users: Vec<usize> = workspace
                .foreign
                .iter()
                .filter(|(dir, _)| crate::workspace::contains(dir, path))
                .flat_map(|(_, users)| users.iter().copied())
                .collect();
            users.sort_unstable();
            users.dedup();
            Effect::PathDependency(users)
        }
        Some(Owner::Member(index)) if !workspace.packages[index].dir.is_empty() => {
            Effect::Package(index)
        }
        // The root package shares its directory with the workspace: it owns only the
        // top-level entries that hold its targets.
        Some(Owner::Member(index)) => {
            let entry = path.split('/').next().unwrap_or_default();
            if workspace.root_entries.iter().any(|owned| owned == entry) {
                Effect::Package(index)
            } else {
                Effect::Outside
            }
        }
    }
}

/// Selects the changed packages and every package that depends on them, directly or not.
/// Any file that is a full-run file or lies outside every package selects the full workspace.
///
/// `symlinks` holds tracked symbolic links as (link, target). A changed file below a target
/// also counts as changed at the link, because the package that holds the link reads it.
/// `special` holds changed paths that are or were a symbolic link or a submodule: their
/// effect on other paths is unknown, so each selects the full workspace.
pub fn select(
    workspace: &Workspace,
    rules: &Rules,
    files: &[String],
    symlinks: &[(String, String)],
    special: &[String],
) -> Selection {
    let mut full_reasons = Vec::new();
    let mut changed = Vec::new();
    let mut classified = Vec::new();
    // Follow the links until no new path appears: a link can point to another link, and a
    // target can be missing. Each round follows every link once, so the rounds are bounded.
    let mut reached: Vec<(String, Option<String>)> =
        files.iter().map(|path| (path.clone(), None)).collect();
    let mut frontier = reached.clone();
    for _ in 0..symlinks.len() {
        let mut next = Vec::new();
        for (path, origin) in &frontier {
            for (link, target) in symlinks {
                // A link to the workspace root or above it reaches every file: the package that
                // holds the link counts as changed.
                let via = if target
                    .split('/')
                    .all(|part| part.is_empty() || part == "..")
                {
                    link.clone()
                } else {
                    let Some(rest) = path.strip_prefix(target.as_str()) else {
                        continue;
                    };
                    if !(rest.is_empty() || rest.starts_with('/')) {
                        continue;
                    }
                    format!("{link}{rest}")
                };
                let new = !reached.iter().any(|(known, _)| *known == via);
                if new {
                    let origin = origin.clone().unwrap_or_else(|| path.clone());
                    reached.push((via.clone(), Some(origin.clone())));
                    next.push((via, Some(origin)));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    reached.sort();
    for (path, link_to) in reached {
        let effect = if special.contains(&path) {
            Effect::FullRun("is a symbolic link, a submodule or a nested repository".to_string())
        } else {
            classify(workspace, rules, &path)
        };
        match &effect {
            Effect::FullRun(why) => full_reasons.push(format!("`{path}` {why}")),
            Effect::Outside => full_reasons.push(format!("`{path}` is outside every package")),
            Effect::Package(index) => changed.push(*index),
            Effect::PathDependency(users) => changed.extend(users),
            Effect::Ignored(_) => {}
        }
        classified.push(File {
            path,
            effect,
            link_to,
        });
    }

    if !full_reasons.is_empty() {
        return Selection {
            full_reasons,
            files: classified,
            packages: (0..workspace.packages.len())
                .map(|index| (index, Reason::Full))
                .collect(),
        };
    }
    Selection {
        full_reasons,
        files: classified,
        packages: dependents_closure(workspace, &changed),
    }
}

/// Breadth-first walk from the changed packages over reverse dependency edges.
fn dependents_closure(workspace: &Workspace, changed: &[usize]) -> Vec<(usize, Reason)> {
    let count = workspace.packages.len();
    let mut dependents = vec![Vec::new(); count];
    for (index, package) in workspace.packages.iter().enumerate() {
        for &dep in &package.deps {
            dependents[dep].push(index);
        }
    }
    // `cargo metadata --no-deps` does not list the dependencies of a path dependency that is
    // not a member. It can depend on any member, so its users depend on every member.
    for (_, users) in &workspace.foreign {
        for (member, member_dependents) in dependents.iter_mut().enumerate() {
            member_dependents.extend(users.iter().filter(|&&user| user != member));
        }
    }
    // A registry package can resolve to a patched member, and other registry packages can
    // depend on it. So every package with registry dependencies depends on a patched member.
    for &patched in &workspace.patched {
        dependents[patched].extend(
            (0..count).filter(|&index| index != patched && workspace.packages[index].external),
        );
    }
    let mut reason: Vec<Option<Reason>> = vec![None; count];
    let mut queue = VecDeque::new();
    for &index in changed {
        if reason[index].is_none() {
            reason[index] = Some(Reason::Changed);
            queue.push_back(index);
        }
    }
    while let Some(index) = queue.pop_front() {
        for &dependent in &dependents[index] {
            if reason[dependent].is_none() {
                reason[dependent] = Some(Reason::DependsOn(index));
                queue.push_back(dependent);
            }
        }
    }
    reason
        .into_iter()
        .enumerate()
        .filter_map(|(index, reason)| reason.map(|reason| (index, reason)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Package;

    fn package(name: &str, dir: &str, deps: &[usize]) -> Package {
        Package {
            name: name.to_string(),
            dir: dir.to_string(),
            deps: deps.to_vec(),
            external: false,
        }
    }

    /// core <- mid <- app; tool is independent.
    fn workspace() -> Workspace {
        Workspace {
            packages: vec![
                package("app", "crates/app", &[2]),
                package("core", "crates/core", &[]),
                package("mid", "crates/mid", &[1]),
                package("tool", "tools/tool", &[]),
            ],
            build_scripts: vec!["crates/core/gen.rs".to_string()],
            full_run: vec!["ci/**".to_string()],
            ignore: vec!["**/*.md".to_string()],
            ..Workspace::default()
        }
    }

    fn names(workspace: &Workspace, selection: &Selection) -> Vec<String> {
        selection
            .packages
            .iter()
            .map(|(index, _)| workspace.packages[*index].name.clone())
            .collect()
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn selects_changed_package_and_transitive_dependents() {
        let ws = workspace();
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core/src/lib.rs"]),
            &[],
            &[],
        );
        assert!(!selection.is_full());
        assert_eq!(names(&ws, &selection), ["app", "core", "mid"]);
        assert_eq!(
            selection.packages,
            [
                (0, Reason::DependsOn(2)),
                (1, Reason::Changed),
                (2, Reason::DependsOn(1))
            ]
        );
    }

    #[test]
    fn leaf_change_selects_only_the_leaf() {
        let ws = workspace();
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/app/tests/it.rs"]),
            &[],
            &[],
        );
        assert_eq!(names(&ws, &selection), ["app"]);
    }

    #[test]
    fn no_changes_select_nothing() {
        let ws = workspace();
        let selection = select(&ws, &Rules::default(), &[], &[], &[]);
        assert!(!selection.is_full());
        assert!(selection.packages.is_empty());
    }

    #[test]
    fn full_run_files_select_everything() {
        let ws = workspace();
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "rust-toolchain",
            ".cargo/config.toml",
            "crates/app/.cargo/config",
            "crates/app/Cargo.toml",
            "crates/app/fuzz/Cargo.lock",
            "crates/app/build.rs",
            "crates/core/gen.rs",
            "ci/run.sh",
        ] {
            let selection = select(&ws, &Rules::default(), &files(&[path]), &[], &[]);
            assert!(selection.is_full(), "{path}");
            assert_eq!(selection.packages.len(), 4, "{path}");
            assert!(selection.packages.iter().all(|(_, r)| *r == Reason::Full));
        }
    }

    #[test]
    fn files_outside_every_package_select_everything() {
        let ws = workspace();
        for path in [
            "scripts/release.sh",
            "crates/notes.txt",
            "../outside/lib.rs",
        ] {
            let selection = select(&ws, &Rules::default(), &files(&[path]), &[], &[]);
            assert!(selection.is_full(), "{path}");
            assert_eq!(selection.files[0].effect, Effect::Outside);
        }
    }

    #[test]
    fn member_manifests_select_everything() {
        let ws = workspace();
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/mid/Cargo.toml"]),
            &[],
            &[],
        );
        assert!(selection.is_full());
    }

    #[test]
    fn ignore_globs_drop_files_but_never_full_run_files() {
        let ws = workspace();
        let selection = select(&ws, &Rules::default(), &files(&["docs/guide.md"]), &[], &[]);
        assert!(!selection.is_full());
        assert!(selection.packages.is_empty());
        let rules = Rules {
            ignore: vec!["**".to_string()],
            ..Rules::default()
        };
        assert!(select(&ws, &rules, &files(&["Cargo.lock"]), &[], &[]).is_full());
        assert!(select(&ws, &rules, &files(&["scripts/x.sh"]), &[], &[])
            .packages
            .is_empty());
    }

    #[test]
    fn command_line_full_run_globs_add_to_the_workspace_ones() {
        let ws = workspace();
        let rules = Rules {
            full_run: vec!["fixtures/**".to_string()],
            ..Rules::default()
        };
        assert!(select(&ws, &rules, &files(&["fixtures/a.json"]), &[], &[]).is_full());
    }

    #[test]
    fn path_dependency_users_depend_on_every_member() {
        let mut ws = workspace();
        // vendor/v is a path dependency of tool and is not a member.
        ws.foreign = vec![("vendor/v".to_string(), vec![3])];
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["vendor/v/src/lib.rs"]),
            &[],
            &[],
        );
        assert_eq!(names(&ws, &selection), ["tool"]);
        // vendor/v may depend on core, so tool may too.
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core/src/lib.rs"]),
            &[],
            &[],
        );
        assert_eq!(names(&ws, &selection), ["app", "core", "mid", "tool"]);
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["tools/tool/src/lib.rs"]),
            &[],
            &[],
        );
        assert_eq!(names(&ws, &selection), ["tool"]);
    }

    #[test]
    fn nested_path_dependencies_merge_their_users() {
        let mut ws = workspace();
        ws.foreign = vec![
            ("vendor".to_string(), vec![0]),
            ("vendor/uuid".to_string(), vec![3]),
        ];
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["vendor/uuid/src/lib.rs"]),
            &[],
            &[],
        );
        assert_eq!(
            selection.files[0].effect,
            Effect::PathDependency(vec![0, 3])
        );
    }

    #[test]
    fn files_reached_through_symlinks_select_the_link_owner() {
        let ws = workspace();
        let links = [
            (
                "crates/app/src/shared.rs".to_string(),
                "tools/tool/src/shared.rs".to_string(),
            ),
            ("crates/mid/data".to_string(), "fixtures".to_string()),
        ];
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["tools/tool/src/shared.rs"]),
            &links,
            &[],
        );
        assert_eq!(names(&ws, &selection), ["app", "tool"]);
        // A directory link: the file is outside every package, so this is a full run anyway.
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["fixtures/a.json"]),
            &links,
            &[],
        );
        assert!(selection.is_full());
        let ignore = Rules {
            ignore: vec!["fixtures/**".to_string()],
            ..Rules::default()
        };
        let selection = select(&ws, &ignore, &files(&["fixtures/a.json"]), &links, &[]);
        assert_eq!(names(&ws, &selection), ["app", "mid"]);
        // A prefix that is not a whole path segment does not reach through the link.
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["tools/tool/src/shared.rsx"]),
            &links,
            &[],
        );
        assert_eq!(names(&ws, &selection), ["tool"]);
    }

    #[test]
    fn deepest_directory_owns_a_file() {
        let mut ws = workspace();
        ws.packages
            .push(package("nested", "crates/core/nested", &[]));
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core/nested/src/lib.rs"]),
            &[],
            &[],
        );
        assert_eq!(names(&ws, &selection), ["nested"]);
        // A directory name that only shares a prefix does not own the file.
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core-extra/src/lib.rs"]),
            &[],
            &[],
        );
        assert!(selection.is_full());
    }

    #[test]
    fn root_package_owns_only_its_target_entries() {
        let mut ws = workspace();
        ws.packages.push(package("root", "", &[0]));
        ws.root_entries = vec!["src".to_string(), "tests".to_string()];
        let root = ws.packages.len() - 1;
        let selection = select(&ws, &Rules::default(), &files(&["src/main.rs"]), &[], &[]);
        assert_eq!(selection.packages, [(root, Reason::Changed)]);
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core/src/lib.rs"]),
            &[],
            &[],
        );
        assert_eq!(names(&ws, &selection), ["app", "core", "mid", "root"]);
        for path in ["README.txt", "crates/notes.txt", ".github/workflows/ci.yml"] {
            assert!(
                select(&ws, &Rules::default(), &files(&[path]), &[], &[]).is_full(),
                "{path}"
            );
        }
    }

    #[test]
    fn chained_links_reach_every_reader() {
        let ws = workspace();
        // app/src/shared.rs -> mid/src/shared.rs -> core/src/shared.rs, which was deleted.
        let links = [
            (
                "crates/app/src/shared.rs".to_string(),
                "crates/mid/src/shared.rs".to_string(),
            ),
            (
                "crates/mid/src/shared.rs".to_string(),
                "crates/core/src/shared.rs".to_string(),
            ),
        ];
        let changed = files(&["crates/core/src/shared.rs"]);
        let selection = select(&ws, &Rules::default(), &changed, &links, &[]);
        assert_eq!(names(&ws, &selection), ["app", "core", "mid"]);
        let origins: Vec<_> = selection
            .files
            .iter()
            .filter_map(|f| f.link_to.as_deref())
            .collect();
        assert_eq!(
            origins,
            ["crates/core/src/shared.rs", "crates/core/src/shared.rs"]
        );
        // A link to the workspace root reaches every file.
        let root_link = [("tools/tool/all".to_string(), String::new())];
        let changed = files(&["crates/core/src/lib.rs"]);
        let selection = select(&ws, &Rules::default(), &changed, &root_link, &[]);
        assert_eq!(names(&ws, &selection), ["app", "core", "mid", "tool"]);
        let parent_link = [("tools/tool/up".to_string(), "..".to_string())];
        let selection = select(&ws, &Rules::default(), &changed, &parent_link, &[]);
        assert_eq!(names(&ws, &selection), ["app", "core", "mid", "tool"]);
        // A directory link inside its own target ends after one round per link.
        let cycle = [("tools/tool/again".to_string(), "tools/tool".to_string())];
        let changed = files(&["tools/tool/x.rs"]);
        let selection = select(&ws, &Rules::default(), &changed, &cycle, &[]);
        assert_eq!(selection.files.len(), 2);
    }

    #[test]
    fn changed_links_and_submodules_select_everything() {
        let ws = workspace();
        let changed = files(&["crates/app/src/current"]);
        let selection = select(&ws, &Rules::default(), &changed, &[], &changed);
        assert!(selection.is_full());
    }

    #[test]
    fn files_next_to_targets_of_other_packages_select_everything() {
        let mut ws = workspace();
        ws.cross_targets = vec![("crates/core/shared".to_string(), "app".to_string())];
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core/shared/check.rs"]),
            &[],
            &[],
        );
        assert!(selection.is_full());
        let selection = select(
            &ws,
            &Rules::default(),
            &files(&["crates/core/src/lib.rs"]),
            &[],
            &[],
        );
        assert!(!selection.is_full());
    }

    #[test]
    fn with_a_patch_packages_with_registry_dependencies_depend_on_every_member() {
        let mut ws = workspace();
        ws.packages[3].external = true; // tool
        let changed = files(&["crates/core/src/lib.rs"]);
        assert_eq!(
            names(&ws, &select(&ws, &Rules::default(), &changed, &[], &[])),
            ["app", "core", "mid"]
        );
        ws.patched = vec![1]; // core
        assert_eq!(
            names(&ws, &select(&ws, &Rules::default(), &changed, &[], &[])),
            ["app", "core", "mid", "tool"]
        );
        // A member that no patch points to keeps its own dependents.
        let changed = files(&["crates/mid/src/lib.rs"]);
        assert_eq!(
            names(&ws, &select(&ws, &Rules::default(), &changed, &[], &[])),
            ["app", "mid"]
        );
    }

    #[test]
    fn dev_dependency_cycles_terminate() {
        let ws = Workspace {
            packages: vec![package("a", "a", &[1]), package("b", "b", &[0])],
            ..Workspace::default()
        };
        let selection = select(&ws, &Rules::default(), &files(&["a/src/lib.rs"]), &[], &[]);
        assert_eq!(
            selection.packages,
            [(0, Reason::Changed), (1, Reason::DependsOn(0))]
        );
    }

    /// A small deterministic generator, so the property test needs no extra crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn property_selection_is_exactly_the_dependents_closure() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..2000 {
            let count = 1 + rng.below(12);
            let packages: Vec<Package> = (0..count)
                .map(|i| {
                    // Any edge is allowed: dev-dependencies can form cycles.
                    let deps: Vec<usize> = (0..count)
                        .filter(|&j| j != i && rng.below(4) == 0)
                        .collect();
                    package(&format!("p{i:02}"), &format!("crates/p{i:02}"), &deps)
                })
                .collect();
            let ws = Workspace {
                packages,
                ..Workspace::default()
            };
            let changed: Vec<usize> = (0..rng.below(4)).map(|_| rng.below(count)).collect();
            let paths: Vec<String> = changed
                .iter()
                .map(|&i| format!("crates/p{i:02}/src/f{}.rs", rng.below(3)))
                .collect();
            let selection = select(&ws, &Rules::default(), &paths, &[], &[]);
            assert!(!selection.is_full(), "case {case}");
            let selected: Vec<bool> = (0..count)
                .map(|i| selection.packages.iter().any(|(j, _)| *j == i))
                .collect();

            // Every changed package is selected.
            for &i in &changed {
                assert!(selected[i], "case {case}: changed p{i:02} missing");
            }
            // Never misses a dependent: the selection is closed under reverse edges.
            for (i, package) in ws.packages.iter().enumerate() {
                for &dep in &package.deps {
                    assert!(
                        !selected[dep] || selected[i],
                        "case {case}: p{i:02} depends on selected p{dep:02}"
                    );
                }
            }
            // Selects nothing else: each selected package reaches a changed one over forward edges.
            for i in (0..count).filter(|&i| selected[i]) {
                let mut seen = vec![false; count];
                let mut stack = vec![i];
                let mut reaches = false;
                while let Some(k) = stack.pop() {
                    if std::mem::replace(&mut seen[k], true) {
                        continue;
                    }
                    reaches |= changed.contains(&k);
                    stack.extend(&ws.packages[k].deps);
                }
                assert!(reaches, "case {case}: p{i:02} selected without a reason");
            }
            // Each `DependsOn` names a selected package that the dependent really depends on.
            for (i, reason) in &selection.packages {
                if let Reason::DependsOn(dep) = reason {
                    assert!(
                        selected[*dep] && ws.packages[*i].deps.contains(dep),
                        "case {case}"
                    );
                }
            }
        }
    }
}
