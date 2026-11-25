//! `cargo tia`: test impact analysis for Cargo workspaces.
//!
//! Selects the workspace packages that a change can affect: the packages that hold changed
//! files and every package that depends on them. Selects the full workspace when it cannot
//! be sure. Makes no network calls.

mod git;
mod glob;
mod select;
mod workspace;

use select::{Effect, Reason, Rules, Selection};
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use workspace::Workspace;

const HELP: &str = "\
cargo-tia: test only the workspace packages that a change can affect

Usage:
  cargo tia [list] [--json] [options]     show the selection
  cargo tia args [options]                print `-p NAME ...` or `--workspace`
  cargo tia test [options] [-- ARGS]      run `cargo test` on the selection
  cargo tia nextest [options] [-- ARGS]   run `cargo nextest run` on the selection

Options:
  --base REV             compare with the merge base of REV and HEAD
                         [default: the default branch of origin]
  --committed            ignore uncommitted and untracked files
  --manifest-path PATH   path to the workspace Cargo.toml
  --full-run GLOB        a changed file matching GLOB selects the full workspace (repeatable)
  --ignore GLOB          ignore changed files matching GLOB (repeatable)
  --json                 print the selection as JSON (list only)
  -h, --help             print this help
  -V, --version          print the version

The selection is the full workspace when Cargo.toml, Cargo.lock, rust-toolchain(.toml),
.cargo/config(.toml), a build script, a file outside every package or a full-run glob
changed, or when git cannot tell what changed. Configure globs in the workspace manifest:

  [workspace.metadata.tia]
  full-run = [\"ci/**\"]
  ignore = [\"docs/**\"]

ARGS after `--` go to the test command, so `cargo tia test -- --release -- --nocapture`
runs `cargo test -p ... --release -- --nocapture`. With an empty selection, test and
nextest run nothing and exit 0.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Args,
    Test,
    Nextest,
}

#[derive(Debug)]
struct Options {
    mode: Mode,
    json: bool,
    base: Option<String>,
    committed: bool,
    manifest_path: Option<PathBuf>,
    rules: Rules,
    passthrough: Vec<OsString>,
}

enum Parsed {
    Run(Options),
    Help,
    Version,
}

fn parse(args: Vec<OsString>) -> Result<Parsed, String> {
    let mut args = args.into_iter().peekable();
    // `cargo tia ...` runs `cargo-tia tia ...`.
    if args.peek().is_some_and(|arg| arg == "tia") {
        args.next();
    }
    let mut options = Options {
        mode: Mode::List,
        json: false,
        base: None,
        committed: false,
        manifest_path: None,
        rules: Rules::default(),
        passthrough: Vec::new(),
    };
    let mut mode_given = false;
    while let Some(arg) = args.next() {
        if arg == "--" {
            options.passthrough = args.by_ref().collect();
            break;
        }
        let arg = arg
            .into_string()
            .map_err(|arg| format!("argument is not UTF-8: {arg:?}"))?;
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or_else(|| format!("{name} needs a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--json" => options.json = true,
            "--committed" => options.committed = true,
            "--base" => options.base = Some(value("--base")?),
            "--manifest-path" => {
                options.manifest_path = Some(PathBuf::from(value("--manifest-path")?))
            }
            "--full-run" => options.rules.full_run.push(value("--full-run")?),
            "--ignore" => options.rules.ignore.push(value("--ignore")?),
            "list" | "args" | "test" | "nextest" if !mode_given => {
                mode_given = true;
                options.mode = match flag.as_str() {
                    "list" => Mode::List,
                    "args" => Mode::Args,
                    "test" => Mode::Test,
                    _ => Mode::Nextest,
                };
            }
            _ => return Err(format!("unexpected argument `{arg}`; see --help")),
        }
        if inline.is_some() && matches!(flag.as_str(), "--json" | "--committed") {
            return Err(format!("{flag} takes no value"));
        }
    }
    // A base that git could read as an option is never a revision.
    if let Some(base) = &options.base {
        if base.is_empty() || base.starts_with('-') {
            return Err(format!("--base needs a revision, not `{base}`"));
        }
    }
    if options.json && options.mode != Mode::List {
        return Err("--json works with `list` only".to_string());
    }
    if !options.passthrough.is_empty() && matches!(options.mode, Mode::List | Mode::Args) {
        return Err("arguments after `--` work with `test` and `nextest` only".to_string());
    }
    Ok(Parsed::Run(options))
}

fn main() -> ExitCode {
    let options = match parse(std::env::args_os().skip(1).collect()) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("cargo-tia {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let workspace = match workspace::load(&cargo, options.manifest_path.as_deref()) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    let (base, selection) =
        match git::changed_files(&workspace.root, options.base.as_deref(), options.committed) {
            Ok(changes) => {
                let selection = select::select(&workspace, &options.rules, &changes.files);
                (Base::Known(changes.base, changes.merge_base), selection)
            }
            Err(unknown) => {
                let selection = Selection::full(&workspace, unknown.reason);
                (Base::Unknown(unknown.base), selection)
            }
        };

    match options.mode {
        Mode::List if options.json => {
            println!("{:#}", to_json(&workspace, &base, &selection));
            ExitCode::SUCCESS
        }
        Mode::List => {
            print!("{}", report(&workspace, &base, &selection));
            ExitCode::SUCCESS
        }
        Mode::Args => {
            println!("{}", cargo_args(&workspace, &selection).join(" "));
            ExitCode::SUCCESS
        }
        Mode::Test | Mode::Nextest => {
            eprint!("{}", report(&workspace, &base, &selection));
            if selection.packages.is_empty() {
                eprintln!("no package is affected; nothing to run");
                return ExitCode::SUCCESS;
            }
            let subcommand: &[&str] = if options.mode == Mode::Test {
                &["test"]
            } else {
                &["nextest", "run"]
            };
            run(
                &cargo,
                subcommand,
                &options,
                &cargo_args(&workspace, &selection),
            )
        }
    }
}

fn run(cargo: &OsStr, subcommand: &[&str], options: &Options, selection: &[String]) -> ExitCode {
    let mut command = Command::new(cargo);
    command.args(subcommand);
    if let Some(path) = &options.manifest_path {
        command.arg("--manifest-path").arg(path);
    }
    command.args(selection).args(&options.passthrough);
    match command.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().map_or(1, |code| code.clamp(1, 255) as u8)),
        Err(error) => {
            eprintln!("error: cannot run cargo: {error}");
            ExitCode::from(2)
        }
    }
}

enum Base {
    /// The base as given or detected, and its merge base with HEAD.
    Known(String, String),
    /// The base, when one was given or detected, that git could not use.
    Unknown(Option<String>),
}

/// `--workspace` for a full selection, else `-p NAME` for each selected package.
fn cargo_args(workspace: &Workspace, selection: &Selection) -> Vec<String> {
    if selection.is_full() {
        return vec!["--workspace".to_string()];
    }
    selection
        .packages
        .iter()
        .flat_map(|(index, _)| ["-p".to_string(), workspace.packages[*index].name.clone()])
        .collect()
}

fn report(workspace: &Workspace, base: &Base, selection: &Selection) -> String {
    let mut out = String::new();
    match base {
        Base::Known(base, merge_base) => {
            let short = &merge_base[..merge_base.len().min(12)];
            out += &format!("base: {base} (merge base {short})\n");
        }
        Base::Unknown(Some(base)) => out += &format!("base: {base} (unusable)\n"),
        Base::Unknown(None) => out += "base: unknown\n",
    }
    if !selection.files.is_empty() {
        out += &format!("changed files: {}\n", selection.files.len());
        let width = selection
            .files
            .iter()
            .map(|(p, _)| p.len())
            .max()
            .unwrap_or(0);
        for (path, effect) in &selection.files {
            let effect = match effect {
                Effect::FullRun(why) => format!("full run: {why}"),
                Effect::Ignored(pattern) => format!("ignored by `{pattern}`"),
                Effect::Package(index) => workspace.packages[*index].name.clone(),
                Effect::PathDependency(users) => format!(
                    "path dependency of {}",
                    users
                        .iter()
                        .map(|i| workspace.packages[*i].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Effect::Outside => "full run: outside every package".to_string(),
            };
            out += &format!("  {path:width$}  {effect}\n");
        }
    }
    let total = workspace.packages.len();
    let noun = if total == 1 { "package" } else { "packages" };
    if selection.is_full() {
        out += &format!("selected: full workspace, {total} {noun}\n");
        for reason in &selection.full_reasons {
            out += &format!("  because {reason}\n");
        }
        return out;
    }
    out += &format!("selected: {} of {total} {noun}\n", selection.packages.len());
    let width = selection
        .packages
        .iter()
        .map(|(i, _)| workspace.packages[*i].name.len())
        .max()
        .unwrap_or(0);
    for (index, reason) in &selection.packages {
        let name = &workspace.packages[*index].name;
        let why = match reason {
            Reason::Full => "full run".to_string(),
            Reason::Changed => "changed".to_string(),
            Reason::DependsOn(dep) => format!("depends on {}", workspace.packages[*dep].name),
        };
        out += &format!("  {name:width$}  {why}\n");
    }
    out
}

fn to_json(workspace: &Workspace, base: &Base, selection: &Selection) -> Value {
    let name = |index: usize| workspace.packages[index].name.clone();
    let (base, merge_base) = match base {
        Base::Known(base, merge_base) => {
            (Value::from(base.clone()), Value::from(merge_base.clone()))
        }
        Base::Unknown(base) => (base.clone().map_or(Value::Null, Value::from), Value::Null),
    };
    let files: Vec<Value> = selection
        .files
        .iter()
        .map(|(path, effect)| match effect {
            Effect::FullRun(why) => json!({"path": path, "effect": "full-run", "detail": why}),
            Effect::Ignored(pattern) => {
                json!({"path": path, "effect": "ignored", "detail": pattern})
            }
            Effect::Package(index) => {
                json!({"path": path, "effect": "package", "packages": [name(*index)]})
            }
            Effect::PathDependency(users) => json!({
                "path": path,
                "effect": "path-dependency",
                "packages": users.iter().map(|i| name(*i)).collect::<Vec<_>>(),
            }),
            Effect::Outside => json!({"path": path, "effect": "outside"}),
        })
        .collect();
    let packages: Vec<Value> = selection
        .packages
        .iter()
        .map(|(index, reason)| match reason {
            Reason::Full => json!({"name": name(*index), "reason": "full"}),
            Reason::Changed => json!({"name": name(*index), "reason": "changed"}),
            Reason::DependsOn(dep) => {
                json!({"name": name(*index), "reason": "dependent", "via": name(*dep)})
            }
        })
        .collect();
    json!({
        "base": base,
        "merge_base": merge_base,
        "full": selection.is_full(),
        "full_reasons": selection.full_reasons,
        "changed_files": files,
        "selected": packages,
        "workspace_packages": workspace.packages.len(),
        "cargo_args": cargo_args(workspace, selection),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Options {
        match parse(args.iter().map(OsString::from).collect()) {
            Ok(Parsed::Run(options)) => options,
            Ok(_) => panic!("expected options"),
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn parses_modes_flags_and_passthrough() {
        let options = parse_ok(&[
            "tia",
            "test",
            "--base",
            "origin/dev",
            "--ignore=docs/**",
            "--",
            "--release",
        ]);
        assert_eq!(options.mode, Mode::Test);
        assert_eq!(options.base.as_deref(), Some("origin/dev"));
        assert_eq!(options.rules.ignore, ["docs/**"]);
        assert_eq!(options.passthrough, [OsString::from("--release")]);
        let options = parse_ok(&[
            "--json",
            "--full-run",
            "a/**",
            "--full-run",
            "b/**",
            "--committed",
        ]);
        assert_eq!(options.mode, Mode::List);
        assert!(options.json && options.committed);
        assert_eq!(options.rules.full_run, ["a/**", "b/**"]);
    }

    #[test]
    fn rejects_bad_combinations() {
        for args in [
            &["args", "--json"][..],
            &["list", "--", "x"],
            &["--base"],
            &["--json=yes"],
            &["--base", "--output=x"],
            &["--base="],
            &["test", "list"],
            &["--bogus"],
        ] {
            assert!(
                parse(args.iter().map(OsString::from).collect()).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn help_and_version() {
        assert!(matches!(
            parse(vec!["tia".into(), "--help".into()]),
            Ok(Parsed::Help)
        ));
        assert!(matches!(parse(vec!["-V".into()]), Ok(Parsed::Version)));
    }
}
