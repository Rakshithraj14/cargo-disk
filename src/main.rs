mod deps;
mod json;
mod metadata;

use metadata::{Metadata, Mode};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const RULE: &str = "────────────────────────────────────";
const WIDE_RULE: &str = "────────────────────────────────────────";
const LABEL_WIDTH: usize = 22;
const CRATE_WIDTH: usize = 30;
const OLD_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const TOP_CRATES: usize = 10;
const MANY_BUILDS: usize = 3;
const MAX_SCAN_DEPTH: usize = 10;

struct Entry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

const USAGE: &str = "\
usage:
  cargo disk                      disk usage of the current project
  cargo disk --all <dir>          every Cargo project under <dir>
  cargo disk deps                 potentially unused and outdated dependencies
  cargo disk clean --incremental  delete incremental caches
                                  [--dry-run] [--yes]
  cargo disk --help | --version";

#[derive(Debug, PartialEq)]
enum Cmd {
    Report,
    All(PathBuf),
    Clean { dry_run: bool, yes: bool },
    Deps,
    Help,
    Version,
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .skip_while(|a| a == "disk")
        .collect();
    let cmd = match parse(&args) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("cargo-disk: {msg}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    std::process::exit(run(cmd));
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let mut rest = args.iter().map(String::as_str);
    match rest.next() {
        None => Ok(Cmd::Report),
        Some("--help" | "-h") => Ok(Cmd::Help),
        Some("--version" | "-V") => Ok(Cmd::Version),
        Some("--all") => match rest.next() {
            Some(dir) if rest.next().is_none() => Ok(Cmd::All(PathBuf::from(dir))),
            Some(_) => Err("--all takes one directory".into()),
            None => Err("--all needs a directory, e.g. `cargo disk --all ~/code`".into()),
        },
        Some("deps") => match rest.next() {
            None => Ok(Cmd::Deps),
            Some(other) => Err(format!("unknown flag for deps: {other}")),
        },
        Some("clean") => {
            let (mut incremental, mut dry_run, mut yes) = (false, false, false);
            for arg in rest {
                match arg {
                    "--incremental" => incremental = true,
                    "--dry-run" => dry_run = true,
                    "--yes" | "-y" => yes = true,
                    other => return Err(format!("unknown flag for clean: {other}")),
                }
            }
            if !incremental {
                return Err(
                    "`clean` needs --incremental. To remove everything, use `cargo clean`".into(),
                );
            }
            Ok(Cmd::Clean { dry_run, yes })
        }
        Some(other) => Err(format!("unknown argument: {other}")),
    }
}

fn run(cmd: Cmd) -> i32 {
    match cmd {
        Cmd::Help => {
            println!("{USAGE}");
            0
        }
        Cmd::Version => {
            println!("cargo-disk {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Cmd::All(dir) => match scan_all(&dir) {
            Some(text) => {
                emit(&text);
                0
            }
            None => {
                eprintln!(
                    "cargo-disk: {} is not a directory — pass the folder that holds your projects, e.g. `cargo disk --all ~/code`",
                    dir.display()
                );
                1
            }
        },
        Cmd::Report | Cmd::Clean { .. } | Cmd::Deps => {
            let Some(manifest) = locate_project() else {
                eprintln!("cargo-disk: not inside a Cargo project");
                return 1;
            };
            let root = manifest.parent().unwrap_or(Path::new(".")).to_path_buf();
            match cmd {
                Cmd::Deps => deps::run(&root),
                Cmd::Clean { dry_run, yes } => {
                    clean_incremental(&output_dirs(&root, None), dry_run, yes)
                }
                _ => {
                    // Offline: a disk report must never touch the network.
                    let full = metadata::load(&root, Mode::Offline).ok();
                    let dirs = output_dirs(&root, full.as_ref());
                    emit(&report(&manifest, &root, &dirs, full.as_ref()));
                    0
                }
            }
        }
    }
}

fn emit(text: &str) {
    let _ = std::io::stdout().write_all(text.as_bytes());
}

/// The directories Cargo writes to: target-dir, plus build-dir when it is
/// set elsewhere. Asked of Cargo rather than guessed, so `build.target-dir`,
/// `build.build-dir` and CARGO_TARGET_DIR are all honored.
fn output_dirs(root: &Path, meta: Option<&Metadata>) -> Vec<PathBuf> {
    let fallback;
    let meta = match meta {
        Some(meta) => Some(meta),
        None => {
            fallback = metadata::load(root, Mode::NoDeps).ok();
            fallback.as_ref()
        }
    };
    let (target, build) = match meta {
        Some(m) => (m.target_dir.clone(), m.build_dir.clone()),
        None => {
            let target = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("target"));
            (target.clone(), target)
        }
    };
    if build.starts_with(&target) {
        vec![target]
    } else if target.starts_with(&build) {
        vec![build]
    } else {
        vec![target, build]
    }
}

/// `path` relative to whichever output directory holds it most closely.
fn relative<'a>(path: &'a Path, dirs: &[PathBuf]) -> Option<&'a Path> {
    dirs.iter()
        .filter_map(|d| path.strip_prefix(d).ok())
        .min_by_key(|rel| rel.components().count())
}

fn report(manifest: &Path, root: &Path, dirs: &[PathBuf], meta: Option<&Metadata>) -> String {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    // build-dir first: target-dir's final binaries are hardlinks into it, and
    // the first name walked keeps the bytes, so they stay under deps/.
    for dir in dirs.iter().rev() {
        walk(dir, &mut files, &mut seen);
    }

    let out = &mut String::new();
    line(out, "Cargo Disk");
    line(out, RULE);
    line(out, "");
    line(out, &format!("Project: {}", project_name(manifest, root)));
    line(out, "");

    let titled: Vec<(&PathBuf, &str, &str)> = dirs
        .iter()
        .zip([("TARGET/", ""), ("BUILD-DIR/", " (build-dir)")])
        .map(|(d, (title, tag))| (d, title, tag))
        .collect();
    let sizes: Vec<u64> = dirs
        .iter()
        .map(|d| {
            files
                .iter()
                .filter(|f| f.path.starts_with(d))
                .map(|f| f.bytes)
                .sum()
        })
        .collect();

    line(out, "Total disk usage");
    for ((dir, _, tag), bytes) in titled.iter().zip(&sizes) {
        row(out, &format!("  {}/{tag}", dir_label(dir)), *bytes);
    }
    if let Ok(meta) = fs::metadata(root.join("Cargo.lock")) {
        row(out, "  Cargo.lock", meta.len());
    }

    let mut largest_profile: Option<(PathBuf, String, u64)> = None;
    for (dir, title, _) in &titled {
        let top = breakdown(&files, dir);
        section(out, title, &top);
        if let Some((label, bytes)) = top.iter().find(|(l, _)| l.ends_with('/')) {
            if largest_profile.as_ref().is_none_or(|(_, _, b)| bytes > b) {
                largest_profile = Some((dir.to_path_buf(), label.clone(), *bytes));
            }
        }
    }
    if let Some((dir, label, _)) = largest_profile {
        let profile = label.trim_end_matches('/');
        section(
            out,
            &format!("{}/", profile.to_uppercase()),
            &breakdown(&files, &dir.join(profile)),
        );
    }

    let crates = group(&files, |f| {
        let rel = relative(&f.path, dirs)?;
        Some(crate_name(rel).unwrap_or_else(|| "(other)".to_string()))
    });
    if !crates.is_empty() {
        line(out, "");
        line(out, "Largest crates");
        line(out, RULE);
        line(out, "");
        for (name, bytes) in crates.iter().take(TOP_CRATES) {
            row_at(out, name, *bytes, CRATE_WIDTH);
        }
    }

    if let Some(meta) = meta {
        let known = known_crates(root, meta);
        let orphans: Vec<(String, u64)> = crates
            .iter()
            .filter(|(name, _)| is_orphan(name, &known))
            .cloned()
            .collect();
        if !orphans.is_empty() {
            let total: u64 = orphans.iter().map(|(_, b)| b).sum();
            section(out, "Potential orphaned crates", &orphans);
            line(out, "");
            row_at(
                out,
                &format!("{} crate{}", orphans.len(), plural(orphans.len())),
                total,
                section_width(&orphans),
            );
        }
    }

    let builds = build_counts(&files, dirs);
    if !builds.is_empty() {
        line(out, "");
        line(out, "Builds per crate");
        line(out, RULE);
        line(out, "");
        for (name, count) in &builds {
            line(
                out,
                &format!("{name:<CRATE_WIDTH$} {count:>4} build{}", plural(*count)),
            );
        }
    }

    let (incremental, old) = cleanup_buckets(&files, dirs, SystemTime::now());
    line(out, "");
    line(out, "Potential cleanup");
    line(out, RULE);
    line(out, "");
    if incremental + old == 0 {
        line(out, "Nothing to clean up.");
    } else {
        for (label, bytes) in [
            ("Incremental cache", incremental),
            ("Artifacts >30 days", old),
        ] {
            if bytes > 0 {
                row(out, label, bytes);
            }
        }
        line(out, "");
        line(out, "Potentially reclaimable:");
        row(out, "", incremental + old);
    }
    out.clone()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
fn locate_project() -> Option<PathBuf> {
    let out = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.is_file().then_some(path)
}

fn project_name(manifest: &Path, root: &Path) -> String {
    fs::read_to_string(manifest)
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("name = "))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| dir_label(root))
}

fn dir_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn walk(dir: &Path, out: &mut Vec<Entry>, seen: &mut HashSet<(u64, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?, e)))
        .collect();
    entries.sort_by_key(|(meta, _)| !meta.is_dir());
    for (meta, entry) in entries {
        if meta.is_dir() {
            walk(&entry.path(), out, seen);
        } else if !already_counted(&meta, seen) {
            out.push(Entry {
                path: entry.path(),
                bytes: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
}

#[cfg(unix)]
fn already_counted(meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn already_counted(_meta: &fs::Metadata, _seen: &mut HashSet<(u64, u64)>) -> bool {
    false
}

fn breakdown(files: &[Entry], base: &Path) -> Vec<(String, u64)> {
    group(files, |f| {
        let mut comps = f.path.strip_prefix(base).ok()?.components();
        let mut label = comps.next()?.as_os_str().to_string_lossy().into_owned();
        if comps.next().is_some() {
            label.push('/');
        }
        Some(label)
    })
}
fn group(files: &[Entry], label: impl Fn(&Entry) -> Option<String>) -> Vec<(String, u64)> {
    let mut sums: HashMap<String, u64> = HashMap::new();
    for f in files {
        if let Some(l) = label(f) {
            *sums.entry(l).or_default() += f.bytes;
        }
    }
    let mut rows: Vec<(String, u64)> = sums.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

fn crate_name(rel: &Path) -> Option<String> {
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let dir = comps.iter().position(|c| {
        matches!(
            c.as_str(),
            "deps" | "examples" | "build" | "incremental" | ".fingerprint"
        )
    });
    let name = match dir {
        Some(i) if i + 1 < comps.len() => {
            let next = &comps[i + 1];
            match comps[i].as_str() {
                "deps" | "examples" => artifact_stem(next)?.rsplit_once('-')?.0,
                _ => next.rsplit_once('-')?.0,
            }
        }
        None if comps.len() == 2 && comps[0] != "package" && !comps[1].starts_with('.') => {
            artifact_stem(&comps[1])?
        }
        _ => return None,
    };
    (!name.is_empty()).then(|| name.replace('-', "_"))
}

fn artifact_stem(file: &str) -> Option<&str> {
    let (stem, ext) = file.split_once('.').unwrap_or((file, ""));
    let stem = match ext {
        "rlib" | "rmeta" | "so" | "a" | "dylib" => stem.strip_prefix("lib").unwrap_or(stem),
        _ => stem,
    };
    (!stem.is_empty()).then_some(stem)
}

fn cleanup_buckets(files: &[Entry], dirs: &[PathBuf], now: SystemTime) -> (u64, u64) {
    let (mut incremental, mut old) = (0, 0);
    for f in files {
        let Some(rel) = relative(&f.path, dirs) else {
            continue;
        };
        if rel.components().any(|c| c.as_os_str() == "incremental") {
            incremental += f.bytes;
        } else if now.duration_since(f.modified).unwrap_or_default() > OLD_AFTER {
            old += f.bytes;
        }
    }
    (incremental, old)
}

// Cargo.lock alone is not enough: it lists package names, while target/ is named
// after lib targets, and the two differ (md-5 builds md5, rustls-webpki builds
// webpki). Full metadata carries both, so the orphan section requires it —
// without it there is no section rather than a wrong one.
fn known_crates(root: &Path, meta: &Metadata) -> HashSet<String> {
    let mut names: HashSet<String> = meta.artifact_names().collect();
    if let Ok(lock) = fs::read_to_string(root.join("Cargo.lock")) {
        names.extend(
            lock.lines()
                .filter_map(|l| l.trim().strip_prefix("name = "))
                .map(|v| v.trim().trim_matches('"').replace('-', "_")),
        );
    }
    names
}

fn is_orphan(name: &str, known: &HashSet<String>) -> bool {
    !known.contains(name) && !matches!(name, "(other)" | "build_script_build" | "build_script_main")
}

fn build_counts(files: &[Entry], dirs: &[PathBuf]) -> Vec<(String, usize)> {
    let mut units: HashMap<String, HashSet<String>> = HashMap::new();
    for f in files {
        let Some(rel) = relative(&f.path, dirs) else {
            continue;
        };
        let comps: Vec<_> = rel.components().map(|c| c.as_os_str()).collect();
        let Some(at) = comps.iter().position(|c| *c == ".fingerprint") else {
            continue;
        };
        let Some(unit) = comps.get(at + 1).map(|c| c.to_string_lossy()) else {
            continue;
        };
        if let Some((name, hash)) = unit.rsplit_once('-') {
            units
                .entry(name.replace('-', "_"))
                .or_default()
                .insert(hash.to_string());
        }
    }
    let mut rows: Vec<(String, usize)> = units
        .into_iter()
        .map(|(name, hashes)| (name, hashes.len()))
        .filter(|(_, n)| *n > MANY_BUILDS)
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

fn scan_all(dir: &Path) -> Option<String> {
    if !dir.is_dir() {
        return None;
    }
    let mut roots = Vec::new();
    find_projects(dir, &mut roots, 0);

    let now = SystemTime::now();
    let mut projects: Vec<(String, u64, SystemTime)> = roots
        .iter()
        .map(|root| {
            let mut files = Vec::new();
            walk(&root.join("target"), &mut files, &mut HashSet::new());
            let bytes = files.iter().map(|f| f.bytes).sum();
            let newest = files
                .iter()
                .map(|f| f.modified)
                .max()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (dir_label(root), bytes, newest)
        })
        .collect();
    projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let out = &mut String::new();
    line(out, "Cargo Disk — Projects");
    line(out, WIDE_RULE);
    line(out, "");
    if projects.is_empty() {
        line(
            out,
            &format!("No Cargo projects with a target/ under {}", dir.display()),
        );
    } else {
        let width = projects
            .iter()
            .map(|(name, _, _)| name.chars().count())
            .max()
            .unwrap_or(0)
            .max(LABEL_WIDTH);
        line(
            out,
            &format!("{:<width$} {:>9}  {}", "Project", "Target", "Last used"),
        );
        for (name, bytes, newest) in &projects {
            line(
                out,
                &format!(
                    "{:<width$} {:>9}  {}",
                    name,
                    format_size(*bytes),
                    ago(*newest, now)
                ),
            );
        }
        line(out, "");
        line(out, &format!("Projects: {}", projects.len()));
        line(
            out,
            &format!(
                "Combined target usage: {}",
                format_size(projects.iter().map(|(_, b, _)| b).sum())
            ),
        );
    }

    if let Some(home) = cargo_home() {
        let caches: Vec<(String, u64)> = ["registry", "git"]
            .iter()
            .filter_map(|name| {
                let mut files = Vec::new();
                walk(&home.join(name), &mut files, &mut HashSet::new());
                let bytes: u64 = files.iter().map(|f| f.bytes).sum();
                (bytes > 0).then(|| (format!("{name}/"), bytes))
            })
            .collect();
        if !caches.is_empty() {
            let total: u64 = caches.iter().map(|(_, b)| b).sum();
            line(out, "");
            line(out, "Cargo home");
            line(out, WIDE_RULE);
            line(out, "");
            for (name, bytes) in &caches {
                row(out, name, *bytes);
            }
            row(out, "Total shared cache:", total);
        }
    }
    Some(out.clone())
}

fn find_projects(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    if dir.join("Cargo.toml").is_file() && dir.join("target").is_dir() {
        out.push(dir.to_path_buf());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        find_projects(&entry.path(), out, depth + 1);
    }
}

fn cargo_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".cargo"))
}

fn ago(then: SystemTime, now: SystemTime) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    let (n, unit) = match secs {
        s if s < 2 * MINUTE => return "just now".to_string(),
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < DAY => (s / HOUR, "hour"),
        s if s < 60 * DAY => (s / DAY, "day"),
        s if s < 365 * DAY => (s / (30 * DAY), "month"),
        s => (s / (365 * DAY), "year"),
    };
    format!("{n} {unit}{} ago", plural(n as usize))
}

fn clean_incremental(outputs: &[PathBuf], dry_run: bool, yes: bool) -> i32 {
    let outputs: Vec<PathBuf> = outputs
        .iter()
        .filter_map(|d| d.canonicalize().ok())
        .collect();
    if outputs.is_empty() {
        eprintln!("cargo-disk: nothing built yet — no target directory");
        return 1;
    }
    let mut dirs: Vec<(PathBuf, u64, &PathBuf)> = Vec::new();
    for output in &outputs {
        for dir in incremental_dirs(output) {
            let mut files = Vec::new();
            walk(&dir, &mut files, &mut HashSet::new());
            let bytes = files.iter().map(|f| f.bytes).sum();
            if bytes > 0 && !dirs.iter().any(|(d, _, _)| *d == dir) {
                dirs.push((dir, bytes, output));
            }
        }
    }
    if dirs.is_empty() {
        println!("No incremental caches found.");
        return 0;
    }

    let total: u64 = dirs.iter().map(|(_, b, _)| b).sum();
    for (dir, bytes, _) in &dirs {
        println!("{:<9}  {}", format_size(*bytes), dir.display());
    }
    if dry_run {
        println!("\nDry run: nothing deleted ({}).", format_size(total));
        return 0;
    }
    if !yes && !confirm(dirs.len(), total) {
        println!("Aborted.");
        return 0;
    }

    let (mut freed, mut failed) = (0, 0);
    for (dir, bytes, output) in &dirs {
        if !removable(dir, output) {
            eprintln!(
                "cargo-disk: skipped {} — changed since listing",
                dir.display()
            );
            failed += 1;
            continue;
        }
        match fs::remove_dir_all(dir) {
            Ok(()) => freed += bytes,
            Err(e) => {
                eprintln!("cargo-disk: {}: {e}", dir.display());
                failed += 1;
            }
        }
    }
    println!("Freed {}.", format_size(freed));
    i32::from(failed > 0)
}

fn confirm(count: usize, total: u64) -> bool {
    println!(
        "\nThis will delete incremental compilation caches.\n\
         Cargo/rustc will recreate them as needed.\n\
         The next build may take longer.\n"
    );
    print!(
        "Delete {count} director{} ({})? [y/N] ",
        if count == 1 { "y" } else { "ies" },
        format_size(total)
    );
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && answer.trim().eq_ignore_ascii_case("y")
}

// target/<profile>/incremental, or target/<triple>/<profile>/incremental. A
// profile directory is one Cargo gave a .fingerprint, which keeps deps/ and
// any other subdirectory named `incremental` out.
fn incremental_dirs(target: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in child_dirs(target) {
        let nested = child_dirs(&dir);
        for profile in std::iter::once(dir.clone()).chain(nested) {
            if !profile.join(".fingerprint").is_dir() {
                continue;
            }
            let candidate = profile.join("incremental");
            if removable(&candidate, target) {
                found.push(candidate);
            }
        }
    }
    found.sort();
    found
}

fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

// ponytail: re-checked immediately before each delete, which narrows the window
// between listing and removal rather than closing it — std has no openat-style
// removal. Fair trade for a cache directory under a path the user owns.
fn removable(path: &Path, target: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && path.file_name().is_some_and(|n| n == "incremental")
        && path
            .canonicalize()
            .is_ok_and(|canon| canon.starts_with(target) && canon != target)
}

fn section(out: &mut String, title: &str, rows: &[(String, u64)]) {
    if rows.is_empty() {
        return;
    }
    line(out, "");
    line(out, title);
    line(out, RULE);
    line(out, "");
    let width = section_width(rows);
    for (label, bytes) in rows {
        row_at(out, label, *bytes, width);
    }
}

fn section_width(rows: &[(String, u64)]) -> usize {
    rows.iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(LABEL_WIDTH)
}

fn row(out: &mut String, label: &str, bytes: u64) {
    row_at(out, label, bytes, LABEL_WIDTH);
}

fn row_at(out: &mut String, label: &str, bytes: u64, width: usize) {
    line(out, &format!("{:<width$} {:>9}", label, format_size(bytes)));
}

fn line(out: &mut String, text: &str) {
    out.push_str(text);
    out.push('\n');
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = bytes as f64;

    if n >= GB {
        format!("{:.2} GB", n / GB)
    } else if n >= MB {
        format!("{:.2} MB", n / MB)
    } else if n >= KB {
        format!("{:.2} KB", n / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, bytes: u64, age: Duration) -> Entry {
        Entry {
            path: PathBuf::from(path),
            bytes,
            modified: SystemTime::UNIX_EPOCH + age,
        }
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn counts_old_incremental_files_once() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(365 * 24 * 60 * 60);
        let files = vec![
            // 40 days old AND incremental: incremental only, never both.
            entry(
                "/t/debug/incremental/foo/bar.bin",
                100,
                Duration::from_secs(0),
            ),
            entry("/t/debug/deps/stale.rlib", 20, Duration::from_secs(0)),
            entry(
                "/t/debug/deps/fresh.rlib",
                7,
                now.duration_since(SystemTime::UNIX_EPOCH).unwrap(),
            ),
        ];
        let (incremental, old) = cleanup_buckets(&files, &[PathBuf::from("/t")], now);
        assert_eq!((incremental, old), (100, 20));
    }

    #[test]
    fn groups_by_first_component() {
        let files = vec![
            entry("/t/debug/deps/a.rlib", 10, Duration::from_secs(0)),
            entry("/t/debug/build/b.o", 5, Duration::from_secs(0)),
            entry("/t/release/deps/c.rlib", 40, Duration::from_secs(0)),
            entry("/t/CACHEDIR.TAG", 1, Duration::from_secs(0)),
        ];
        let rows = breakdown(&files, Path::new("/t"));
        assert_eq!(
            rows,
            vec![
                ("release/".to_string(), 40),
                ("debug/".to_string(), 15),
                ("CACHEDIR.TAG".to_string(), 1),
            ]
        );
    }

    #[test]
    fn maps_files_to_crates() {
        let cases = [
            ("debug/deps/libtokio-3f0ef8f6133b01c4.rlib", Some("tokio")),
            ("debug/deps/libtokio-3f0ef8f6133b01c4.rmeta", Some("tokio")),
            ("debug/deps/tokio-3f0ef8f6133b01c4.d", Some("tokio")),
            ("debug/deps/liblibc-aaaa.rlib", Some("libc")),
            ("debug/deps/cargo_disk-5acc4a81361ae9c9", Some("cargo_disk")),
            ("debug/cargo-disk", Some("cargo_disk")),
            ("debug/build/proc-macro2-abc/out/x.rs", Some("proc_macro2")),
            (
                "debug/incremental/cargo_disk-0cb12/s-x/dep-graph.bin",
                Some("cargo_disk"),
            ),
            ("debug/.fingerprint/serde-1234/lib-serde", Some("serde")),
            (
                "x86_64-unknown-linux-gnu/release/deps/libfoo-12.rlib",
                Some("foo"),
            ),
            ("debug/examples/demo-99", Some("demo")),
            ("debug/.cargo-lock", None),
            ("CACHEDIR.TAG", None),
            ("package/cargo-disk-0.1.0.crate", None),
        ];
        for (path, want) in cases {
            assert_eq!(crate_name(Path::new(path)).as_deref(), want, "{path}");
        }
    }

    #[test]
    fn parses_every_command_form() {
        let parse = |s: &str| {
            parse(
                &s.split_whitespace()
                    .map(String::from)
                    .collect::<Vec<String>>(),
            )
        };
        assert_eq!(parse(""), Ok(Cmd::Report));
        assert_eq!(parse("--help"), Ok(Cmd::Help));
        assert_eq!(parse("--version"), Ok(Cmd::Version));
        assert_eq!(parse("--all /code"), Ok(Cmd::All(PathBuf::from("/code"))));
        assert_eq!(
            parse("clean --incremental"),
            Ok(Cmd::Clean {
                dry_run: false,
                yes: false
            })
        );
        assert_eq!(
            parse("clean --incremental --dry-run --yes"),
            Ok(Cmd::Clean {
                dry_run: true,
                yes: true
            })
        );
        for bad in ["--json", "--all", "--all a b", "clean", "clean --all"] {
            assert!(parse(bad).is_err(), "{bad} must be rejected");
        }
        assert!(parse("clean").unwrap_err().contains("cargo clean"));
    }

    #[test]
    fn orphans_are_crates_absent_from_the_lockfile() {
        let locked: HashSet<String> = ["serde", "serde_core", "my_crate"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(is_orphan("regex", &locked));
        assert!(!is_orphan("serde", &locked));
        assert!(!is_orphan("my_crate", &locked));
        for pseudo in ["(other)", "build_script_build", "build_script_main"] {
            assert!(!is_orphan(pseudo, &locked), "{pseudo}");
        }
    }

    #[test]
    fn lib_names_count_as_known_not_just_package_names() {
        let dir = std::env::temp_dir().join("cargo-disk-known-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The lockfile only says md-5; target/ says md5. On a freshly built
        // clob-rs this invented three orphans (md5, webpki, utf8).
        fs::write(dir.join("Cargo.lock"), "[[package]]\nname = \"md-5\"\n").unwrap();
        let meta = metadata::from_json(
            &json::parse(
                r#"{"target_directory":"/t","packages":[{"id":"m","name":"md-5",
                "manifest_path":"/m/Cargo.toml","dependencies":[],
                "targets":[{"kind":["lib"],"name":"md5"}]}]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let known = known_crates(&dir, &meta);
        assert!(!is_orphan("md5", &known));
        assert!(!is_orphan("md_5", &known));
        assert!(is_orphan("regex", &known));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_are_relative_to_the_closest_output_dir() {
        let dirs = [PathBuf::from("/p/out"), PathBuf::from("/p/out/bld")];
        assert_eq!(
            relative(Path::new("/p/out/bld/debug/deps/x"), &dirs),
            Some(Path::new("debug/deps/x"))
        );
        assert_eq!(
            relative(Path::new("/p/out/debug/app"), &dirs),
            Some(Path::new("debug/app"))
        );
        assert_eq!(relative(Path::new("/elsewhere/x"), &dirs), None);
    }

    #[test]
    fn counts_fingerprint_units_per_crate() {
        let unit = |hash: &str| {
            entry(
                &format!("/t/debug/.fingerprint/cargo-disk-{hash}/lib-cargo-disk"),
                1,
                Duration::from_secs(0),
            )
        };
        let mut files = vec![unit("aaa"), unit("bbb"), unit("ccc")];
        assert!(
            build_counts(&files, &[PathBuf::from("/t")]).is_empty(),
            "three units is normal: lib, test, check"
        );
        files.push(unit("ddd"));
        files.push(entry(
            "/t/debug/deps/libserde-1.rlib",
            1,
            Duration::from_secs(0),
        ));
        assert_eq!(
            build_counts(&files, &[PathBuf::from("/t")]),
            vec![("cargo_disk".to_string(), 4)]
        );
    }

    #[test]
    fn only_incremental_dirs_inside_target_are_removable() {
        let target = std::env::temp_dir().join("cargo-disk-clean-test");
        let _ = fs::remove_dir_all(&target);
        let good = target.join("debug/incremental");
        fs::create_dir_all(&good).unwrap();
        fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
        // Not a profile directory, so its `incremental` is not a cache.
        fs::create_dir_all(target.join("debug/deps/incremental")).unwrap();
        fs::create_dir_all(target.join("debug/build")).unwrap();
        // The cross-compilation layout: target/<triple>/<profile>/incremental.
        let triple = target.join("x86_64-unknown-linux-gnu/release");
        fs::create_dir_all(triple.join(".fingerprint")).unwrap();
        fs::create_dir_all(triple.join("incremental")).unwrap();
        let outside = std::env::temp_dir().join("cargo-disk-clean-outside/incremental");
        fs::create_dir_all(&outside).unwrap();

        let canon = target.canonicalize().unwrap();
        assert_eq!(
            incremental_dirs(&canon),
            vec![good.clone(), triple.join("incremental")]
        );
        assert!(removable(&good, &canon));
        assert!(
            !removable(&target.join("debug/build"), &canon),
            "wrong name"
        );
        assert!(!removable(&outside, &canon), "outside target");
        assert!(!removable(&target.join("debug/nope"), &canon), "missing");

        #[cfg(unix)]
        {
            let link = target.join("release/incremental");
            fs::create_dir_all(target.join("release/.fingerprint")).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(!removable(&link, &canon), "symlinks are never removed");
            assert_eq!(
                incremental_dirs(&canon),
                vec![good, triple.join("incremental")],
                "a symlinked cache is skipped, not followed"
            );
        }

        fs::remove_dir_all(&target).unwrap();
        fs::remove_dir_all(outside.parent().unwrap()).unwrap();
    }

    #[test]
    fn describes_age_in_the_largest_useful_unit() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(400 * 24 * 3600);
        let at = |secs: u64| now - Duration::from_secs(secs);
        assert_eq!(ago(at(30), now), "just now");
        assert_eq!(ago(at(600), now), "10 minutes ago");
        assert_eq!(ago(at(3600), now), "1 hour ago");
        assert_eq!(ago(at(2 * 24 * 3600), now), "2 days ago");
        assert_eq!(ago(at(120 * 24 * 3600), now), "4 months ago");
        assert_eq!(ago(at(370 * 24 * 3600), now), "1 year ago");
    }

    #[test]
    fn finds_projects_without_descending_into_target() {
        let base = std::env::temp_dir().join("cargo-disk-scan-test");
        let _ = fs::remove_dir_all(&base);
        for dir in ["a/target/debug", "nested/b/target", ".hidden/c/target"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        for manifest in [
            "a/Cargo.toml",
            "nested/b/Cargo.toml",
            ".hidden/c/Cargo.toml",
        ] {
            fs::write(base.join(manifest), "[package]").unwrap();
        }
        // A vendored manifest inside a build tree is not a project.
        fs::write(base.join("a/target/debug/Cargo.toml"), "[package]").unwrap();
        fs::create_dir_all(base.join("a/target/debug/target")).unwrap();
        // Built nothing: no target/, so not listed.
        fs::create_dir_all(base.join("unbuilt")).unwrap();
        fs::write(base.join("unbuilt/Cargo.toml"), "[package]").unwrap();

        let mut found = Vec::new();
        find_projects(&base, &mut found, 0);
        found.sort();
        assert_eq!(found, vec![base.join("a"), base.join("nested/b")]);

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn walk_sums_a_real_directory_counting_hardlinks_once() {
        let dir = std::env::temp_dir().join("cargo-disk-walk-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("a.bin"), [0u8; 10]).unwrap();
        fs::write(dir.join("nested/b.bin"), [0u8; 32]).unwrap();
        // What Cargo does to debug/<bin> and debug/deps/<bin>-<hash>.
        fs::hard_link(dir.join("a.bin"), dir.join("nested/a-link.bin")).unwrap();

        let mut files = Vec::new();
        walk(&dir, &mut files, &mut HashSet::new());
        assert_eq!(files.len(), 2, "the hardlinked twin must not be counted");
        assert!(
            files.iter().any(|f| f.path.ends_with("nested/a-link.bin")),
            "the name inside the subdirectory keeps the bytes, like deps/<bin>-<hash>"
        );
        assert_eq!(files.iter().map(|f| f.bytes).sum::<u64>(), 42);

        fs::remove_dir_all(&dir).unwrap();
    }
}
