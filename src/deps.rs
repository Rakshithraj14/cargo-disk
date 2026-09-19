use crate::metadata::{self, Kind, Metadata, Mode, Package};
use crate::{Entry, WIDE_RULE, crate_name, format_size, line, output_dirs, relative, walk};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Unused {
    member: String,
    manifest: PathBuf,
    /// The key in Cargo.toml: the rename if there is one, else the package name.
    key: String,
    package: String,
    kind: Kind,
    target: Option<String>,
    freed: u64,
}

pub fn run(root: &Path) -> i32 {
    let meta = match metadata::load(root, Mode::Online) {
        Ok(meta) => meta,
        Err(e) => {
            eprintln!(
                "cargo-disk: {e}\ncargo-disk: `deps` needs a resolved Cargo.lock — run `cargo build` first"
            );
            return 1;
        }
    };

    let has_deps = meta
        .members
        .iter()
        .filter_map(|id| meta.package(id))
        .any(|p| !p.deps.is_empty());
    if !has_deps {
        println!("This project has no dependencies.");
        return 0;
    }

    let dirs = output_dirs(root, Some(&meta));
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for dir in dirs.iter().rev() {
        walk(dir, &mut files, &mut seen);
    }
    let sizes = crate_sizes(&files, &dirs);

    let (unused, ignored) = find_unused(&meta, &sizes);
    let direct: HashSet<&str> = meta
        .members
        .iter()
        .filter_map(|id| meta.package(id))
        .flat_map(|p| p.deps.iter().map(|d| d.name.as_str()))
        .collect();
    // The first run on a machine fetches index entries for every crate in the
    // graph — 15s for clob-rs's 485 — so say why it is waiting.
    eprintln!("Checking crates.io for newer versions...");
    let outdated = outdated(root).map(|all| {
        all.into_iter()
            .filter(|(name, _, _)| direct.contains(name.as_str()))
            .collect::<Vec<_>>()
    });

    let out = &mut String::new();
    render(
        out,
        &unused,
        ignored,
        outdated.as_deref(),
        meta.members.len() > 1,
    );
    let _ = std::io::stdout().write_all(out.as_bytes());

    if unused.is_empty() || !std::io::stdin().is_terminal() {
        return 0;
    }
    remove_chosen(root, &unused)
}

fn crate_sizes(files: &[Entry], dirs: &[PathBuf]) -> HashMap<String, u64> {
    let mut sizes = HashMap::new();
    for f in files {
        if let Some(name) = relative(&f.path, dirs).and_then(crate_name) {
            *sizes.entry(name).or_default() += f.bytes;
        }
    }
    sizes
}

fn find_unused(meta: &Metadata, sizes: &HashMap<String, u64>) -> (Vec<Unused>, usize) {
    let (mut unused, mut ignored) = (Vec::new(), 0);
    for member in meta.members.iter().filter_map(|id| meta.package(id)) {
        let Some(node) = meta.node(&member.id) else {
            continue;
        };
        let dir = member.manifest_path.parent().unwrap_or(Path::new("."));
        let source = rust_sources(dir);
        let mut seen = HashSet::new();

        for dep in &member.deps {
            let key = dep.rename.clone().unwrap_or_else(|| dep.name.clone());
            if !seen.insert((key.clone(), dep.kind, dep.target.clone())) {
                continue;
            }
            if member.ignore.iter().any(|i| *i == key || *i == dep.name) {
                ignored += 1;
                continue;
            }
            // The resolved edge for this manifest entry. None means it was
            // never activated (an optional dep with its feature off), so there
            // is nothing built to judge.
            let Some(edge) = node.deps.iter().find(|nd| {
                nd.kinds.contains(&dep.kind)
                    && meta.package(&nd.pkg).is_some_and(|p| p.name == dep.name)
                    && dep
                        .rename
                        .as_ref()
                        .is_none_or(|r| r.replace('-', "_") == nd.name)
            }) else {
                continue;
            };
            if uses(&source, &edge.name) {
                continue;
            }
            unused.push(Unused {
                member: member.name.clone(),
                manifest: member.manifest_path.clone(),
                key,
                package: dep.name.clone(),
                kind: dep.kind,
                target: dep.target.clone(),
                freed: freed_if_removed(meta, member, &edge.pkg, sizes),
            });
        }
    }
    (unused, ignored)
}

/// Every .rs file of one package, concatenated. Skips build output, hidden
/// directories and nested packages, which have their own dependencies.
fn rust_sources(dir: &Path) -> String {
    let mut text = String::new();
    collect_sources(dir, dir, &mut text);
    text
}

fn collect_sources(root: &Path, dir: &Path, text: &mut String) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            let nested = path != root && path.join("Cargo.toml").is_file();
            if !(name.starts_with('.') || name == "target" || nested) {
                collect_sources(root, &path, text);
            }
        } else if name.ends_with(".rs") {
            if let Ok(src) = fs::read_to_string(&path) {
                text.push_str(&src);
                text.push('\n');
            }
        }
    }
}

// ponytail: a text search, the same approach as cargo-machete. It cannot see a
// crate that is only linked (`-sys`) or only enables a feature, which is what the
// ignore list is for. A real answer needs the compiler (cargo-udeps, nightly).
fn uses(source: &str, ident: &str) -> bool {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = source.as_bytes();
    source.match_indices(ident).any(|(at, _)| {
        let end = at + ident.len();
        if (at > 0 && is_ident(bytes[at - 1])) || bytes.get(end).is_some_and(|b| is_ident(*b)) {
            return false;
        }
        let before = source[..at].trim_end();
        let word_before = |w: &str| {
            before
                .strip_suffix(w)
                .is_some_and(|rest| rest.as_bytes().last().is_none_or(|b| !is_ident(*b)))
        };
        source[end..].trim_start().starts_with("::")
            || word_before("use")
            || (word_before("crate") && before[..before.len() - 5].trim_end().ends_with("extern"))
    })
}

/// Bytes in target/ belonging to crates that only this dependency pulls in:
/// everything reachable from the workspace now, minus what stays reachable
/// once the edge is gone. Approximate — dropping a crate can change the
/// features of shared ones — hence the `~` in the output.
fn freed_if_removed(
    meta: &Metadata,
    member: &Package,
    dep_pkg: &str,
    sizes: &HashMap<String, u64>,
) -> u64 {
    // Still wired in under another kind (e.g. both [dependencies] and
    // [dev-dependencies]): removing this entry frees nothing.
    let edges = member
        .deps
        .iter()
        .filter(|d| meta.package(dep_pkg).is_some_and(|p| p.name == d.name))
        .count();
    if edges > 1 {
        return 0;
    }

    let graph: HashMap<&str, Vec<&str>> = meta
        .nodes
        .iter()
        .map(|n| {
            (
                n.id.as_str(),
                n.deps.iter().map(|d| d.pkg.as_str()).collect(),
            )
        })
        .collect();
    let reach = |skip: Option<(&str, &str)>| {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = meta.members.iter().map(String::as_str).collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            for &next in graph.get(id).into_iter().flatten() {
                if skip != Some((id, next)) {
                    stack.push(next);
                }
            }
        }
        seen
    };
    let all = reach(None);
    let kept = reach(Some((member.id.as_str(), dep_pkg)));

    // Artifact names still owned by a kept package are shared, e.g. two
    // versions of rand both building `rand`: count none of those bytes rather
    // than guess, so the estimate can only be low, never high.
    let kept_names: HashSet<String> = kept
        .iter()
        .filter_map(|id| meta.package(id))
        .flat_map(Package::artifact_names)
        .collect();
    let freed_names: HashSet<String> = all
        .difference(&kept)
        .filter_map(|id| meta.package(id))
        .flat_map(Package::artifact_names)
        .filter(|n| !kept_names.contains(n))
        .collect();
    freed_names.iter().filter_map(|n| sizes.get(n)).sum()
}

/// (package, current, available) from `cargo update --dry-run --verbose`,
/// which checks the registry without touching Cargo.lock. None when it could
/// not run, typically offline.
fn outdated(root: &Path) -> Option<Vec<(String, String, String)>> {
    let out = Command::new("cargo")
        .args(["update", "--dry-run", "--verbose"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_update(&String::from_utf8_lossy(&out.stderr)))
}

fn parse_update(text: &str) -> Vec<(String, String, String)> {
    let mut found: Vec<(String, String, String)> = text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if let Some(rest) = l.strip_prefix("Unchanged ") {
                let (name, rest) = rest.split_once(' ')?;
                let (current, rest) = rest.split_once(" (available: ")?;
                let available = rest.strip_suffix(')')?;
                Some((name.into(), current.into(), available.into()))
            } else {
                let rest = l.strip_prefix("Updating ")?;
                let (name, rest) = rest.split_once(' ')?;
                let (current, available) = rest.split_once(" -> ")?;
                Some((name.into(), current.into(), available.into()))
            }
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

fn render(
    out: &mut String,
    unused: &[Unused],
    ignored: usize,
    outdated: Option<&[(String, String, String)]>,
    workspace: bool,
) {
    line(out, "Cargo Disk — Dependencies");
    line(out, WIDE_RULE);
    line(out, "");

    let labels: Vec<String> = unused
        .iter()
        .map(|u| {
            let mut label = if workspace {
                format!("{}: {}", u.member, u.key)
            } else {
                u.key.clone()
            };
            if u.key != u.package {
                label.push_str(&format!(" ({})", u.package));
            }
            match u.kind {
                Kind::Dev => label.push_str("  dev"),
                Kind::Build => label.push_str("  build"),
                Kind::Normal => {}
            }
            label
        })
        .collect();
    let width = labels
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(24);

    if unused.is_empty() {
        line(out, "No potentially unused dependencies.");
    } else {
        line(
            out,
            &format!(
                "Potentially unused{:>w$}",
                "freed if removed",
                w = width + 1
            ),
        );
        for (i, (u, label)) in unused.iter().zip(&labels).enumerate() {
            let freed = if u.freed == 0 {
                "—".to_string()
            } else {
                format!("~{}", format_size(u.freed))
            };
            line(out, &format!("{:>3}  {label:<width$}  {freed:>12}", i + 1));
        }
        line(out, "");
        line(
            out,
            "Found by searching the source for `name::`, `use name` and",
        );
        line(
            out,
            "`extern crate name`. A crate that is only linked (often `-sys`)",
        );
        line(
            out,
            "or only enables a feature shows up here too; list it under",
        );
        line(
            out,
            "[package.metadata.cargo-disk] ignore = [\"name\"] in Cargo.toml.",
        );
    }
    if ignored > 0 {
        line(out, "");
        line(
            out,
            &format!(
                "{ignored} dependenc{} ignored via [package.metadata.cargo-disk]",
                if ignored == 1 { "y" } else { "ies" }
            ),
        );
    }

    line(out, "");
    line(out, "Outdated (direct dependencies)");
    match outdated {
        None => line(out, "  couldn't check for updates (offline?)"),
        Some([]) => line(out, "  all up to date"),
        Some(list) => {
            let width = list.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0);
            let cur = list.iter().map(|(_, c, _)| c.len()).max().unwrap_or(0);
            for (name, current, available) in list {
                line(
                    out,
                    &format!("     {name:<width$}  {current:<cur$} → {available}"),
                );
            }
        }
    }
    line(out, "");
}

fn remove_chosen(root: &Path, unused: &[Unused]) -> i32 {
    print!("Remove which? (e.g. 1,2 / none) ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return 1;
    }
    let chosen = match parse_selection(&answer, unused.len()) {
        Ok(chosen) => chosen,
        Err(e) => {
            eprintln!("cargo-disk: {e} — nothing removed");
            return 2;
        }
    };
    if chosen.is_empty() {
        println!("Nothing removed.");
        return 0;
    }

    let mut failed = false;
    let mut touched: Vec<String> = Vec::new();
    for i in chosen {
        let u = &unused[i - 1];
        let mut cmd = Command::new("cargo");
        cmd.arg("remove").arg("--manifest-path").arg(&u.manifest);
        match u.kind {
            Kind::Dev => {
                cmd.arg("--dev");
            }
            Kind::Build => {
                cmd.arg("--build");
            }
            Kind::Normal => {}
        }
        if let Some(target) = &u.target {
            cmd.args(["--target", target]);
        }
        cmd.arg(&u.key);
        let manifest = u.manifest.strip_prefix(root).unwrap_or(&u.manifest);
        let manifest = manifest.display().to_string();
        if !touched.contains(&manifest) {
            touched.push(manifest);
        }
        println!("Running: cargo remove {}", u.key);
        if !cmd.status().is_ok_and(|s| s.success()) {
            eprintln!("cargo-disk: removing {} failed", u.key);
            failed = true;
        }
    }
    touched.push("Cargo.lock".to_string());
    println!("\nRun `cargo build` to confirm it still compiles.");
    println!("Undo with `git checkout {}`.", touched.join(" "));
    i32::from(failed)
}

/// "1,3" / "1 3" / "none" / "" → 1-based indices, sorted and unique.
fn parse_selection(input: &str, max: usize) -> Result<Vec<usize>, String> {
    let input = input.trim();
    if input.is_empty() || input.eq_ignore_ascii_case("none") || input.eq_ignore_ascii_case("n") {
        return Ok(Vec::new());
    }
    let mut picked = Vec::new();
    for part in input.split([',', ' ']).filter(|p| !p.is_empty()) {
        match part.parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => picked.push(n),
            _ => return Err(format!("`{part}` is not a number from 1 to {max}")),
        }
    }
    picked.sort();
    picked.dedup();
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_ways_code_names_a_crate() {
        let used = [
            "use regex::Regex;",
            "let r = regex::Regex::new(x);",
            "pub use regex;",
            "use regex as re;",
            "extern crate regex;",
            "#[macro_use]\nextern crate regex;",
            "#[derive(serde::Serialize)] struct S;",
        ];
        for src in used {
            let ident = if src.contains("serde") {
                "serde"
            } else {
                "regex"
            };
            assert!(uses(src, ident), "{src:?} uses {ident}");
        }
        let unused = [
            "",
            "let regex_count = 1;",
            "fn my_regex() {}",
            "// see the regex docs",
            "let reuse = 1; reuse regex",
            "mod regexes;",
        ];
        for src in unused {
            assert!(!uses(src, "regex"), "{src:?} does not use regex");
        }
    }

    #[test]
    fn reads_cargo_update_dry_run_output() {
        let text = "    Updating crates.io index
     Locking 0 packages to latest compatible versions
   Unchanged rand v0.7.3 (available: v0.10.2)
    Updating serde v1.0.100 -> v1.0.229
   Unchanged rand v0.7.3 (available: v0.10.2)
note: to see how you depend on a package, run `cargo tree --invert <dep>@<ver>`
warning: not updating lockfile due to dry run";
        assert_eq!(
            parse_update(text),
            vec![
                ("rand".into(), "v0.7.3".into(), "v0.10.2".into()),
                ("serde".into(), "v1.0.100".into(), "v1.0.229".into()),
            ]
        );
    }

    #[test]
    fn parses_the_removal_prompt() {
        assert_eq!(parse_selection("", 3), Ok(vec![]));
        assert_eq!(parse_selection(" none\n", 3), Ok(vec![]));
        assert_eq!(parse_selection("3,1 1", 3), Ok(vec![1, 3]));
        assert_eq!(parse_selection("2\n", 3), Ok(vec![2]));
        for bad in ["0", "4", "1,x", "-1", "all"] {
            assert!(parse_selection(bad, 3).is_err(), "{bad}");
        }
    }
}
