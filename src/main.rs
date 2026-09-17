//! `cargo disk` — where your `target/` directory went.
//!
//! Read-only: this tool never deletes anything.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const RULE: &str = "────────────────────────────────────";
const LABEL_WIDTH: usize = 22;
const CRATE_WIDTH: usize = 30;
const OLD_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const TOP_CRATES: usize = 10;

struct Entry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn main() {
    // Cargo invokes us as `cargo-disk disk`; drop the subcommand it inserts.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .skip_while(|a| a == "disk")
        .collect();
    if !args.is_empty() {
        eprintln!("usage: cargo disk        (no options yet)");
        std::process::exit(2);
    }

    let Some(manifest) = locate_project() else {
        eprintln!("cargo-disk: not inside a Cargo project");
        std::process::exit(1);
    };
    let root = manifest.parent().unwrap_or(Path::new(".")).to_path_buf();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    // ponytail: `build.target-dir` in .cargo/config.toml is ignored. Swap in
    // `cargo metadata` (and its ~25 transitive crates) if anyone actually hits this.

    let mut files = Vec::new();
    walk(&target, &mut files, &mut HashSet::new());
    let total: u64 = files.iter().map(|f| f.bytes).sum();

    let out = &mut String::new();
    line(out, "Cargo Disk");
    line(out, RULE);
    line(out, "");
    line(out, &format!("Project: {}", project_name(&manifest, &root)));
    line(out, "");

    line(out, "Total disk usage");
    row(out, &format!("  {}/", dir_label(&target)), total);
    if let Ok(meta) = fs::metadata(root.join("Cargo.lock")) {
        row(out, "  Cargo.lock", meta.len());
    }

    let top = breakdown(&files, &target);
    section(out, "TARGET/", &top);

    // Break the largest profile down one level further (debug/, release/, ...).
    if let Some((label, _)) = top.first().filter(|(l, _)| l.ends_with('/')) {
        let profile = label.trim_end_matches('/');
        let dir = target.join(profile);
        section(
            out,
            &format!("{}/", profile.to_uppercase()),
            &breakdown(&files, &dir),
        );
    }

    // Every profile and target triple summed, so a crate built for both debug
    // and release is one row: what that dependency costs you in total.
    let crates = group(&files, |f| {
        let rel = f.path.strip_prefix(&target).ok()?;
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

    let (incremental, old) = cleanup_buckets(&files, &target, SystemTime::now());
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

    // One write, and a closed pipe (`cargo disk | head`) is a normal exit
    // rather than the panic `println!` would raise on every line after it.
    let _ = std::io::stdout().write_all(out.as_bytes());
}

/// Path to the workspace root `Cargo.toml`, or `None` if we are not in a project.
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

/// The manifest's `name`, falling back to the directory name for a virtual
/// workspace manifest, which has no `[package]` at all.
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

/// Collect every file below `dir`. Unreadable entries are skipped rather than
/// aborting the scan — a partial answer beats no answer for a disk report.
/// `DirEntry::metadata` does not follow symlinks, so symlink loops are safe.
///
/// `seen` holds the inodes of files with more than one link, so hardlinked
/// content is counted once, the way `du` counts it. Cargo hardlinks
/// `debug/<bin>` to `debug/deps/<bin>-<hash>` and rustc hardlinks unchanged
/// object files between incremental sessions; counting both overstated this
/// crate's own `target/` by 25%.
///
/// Subdirectories are walked before the files beside them, so of two names
/// for one file the deeper `deps/<bin>-<hash>` is the one that keeps the bytes
/// and `debug/<bin>` is the duplicate. Otherwise `deps/` came up short by the
/// size of every binary, depending on `read_dir` order.
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

/// True when this file's content has already been counted under another name.
/// Single-link files skip the set entirely — that is nearly all of them.
#[cfg(unix)]
fn already_counted(meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino()))
}

// ponytail: Windows hardlinks exist but std exposes no stable inode, so we
// count every name. Use `File::open` + `GetFileInformationByHandle` if it
// ever matters there.
#[cfg(not(unix))]
fn already_counted(_meta: &fs::Metadata, _seen: &mut HashSet<(u64, u64)>) -> bool {
    false
}

/// Group files by their first path component below `base`, largest first.
/// A label ends in `/` when it is a directory rather than a loose file.
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

/// Sum file sizes per label, largest first. Files labelled `None` are left out.
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

/// The crate a file under `target/` belongs to, from Cargo's naming scheme:
///
/// ```text
/// debug/deps/libtokio-<hash>.rlib     debug/incremental/tokio-<hash>/...
/// debug/deps/tokio-<hash>.d           debug/build/proc-macro2-<hash>/...
/// debug/examples/demo-<hash>          debug/.fingerprint/tokio-<hash>/...
/// debug/cargo-disk                    (uplifted copy, no hash)
/// ```
///
/// Names are normalised to underscores, the form rustc uses, so `proc-macro2`
/// from `build/` and `proc_macro2` from `deps/` land in the same row.
/// `None` for anything that fits no pattern (lock files, `package/`, ...).
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
        // Uplifted `debug/<bin>` / `debug/libfoo.rlib`. Its content is usually
        // a hardlink already counted under deps/, but whichever name the walk
        // reaches first keeps the bytes, so it must map to the same crate.
        None if comps.len() == 2 && comps[0] != "package" && !comps[1].starts_with('.') => {
            artifact_stem(&comps[1])?
        }
        _ => return None,
    };
    (!name.is_empty()).then(|| name.replace('-', "_"))
}

/// `libfoo-<hash>.rlib` -> `foo-<hash>`, `foo-<hash>.d` -> `foo-<hash>`.
fn artifact_stem(file: &str) -> Option<&str> {
    let (stem, ext) = file.split_once('.').unwrap_or((file, ""));
    let stem = match ext {
        "rlib" | "rmeta" | "so" | "a" | "dylib" => stem.strip_prefix("lib").unwrap_or(stem),
        _ => stem,
    };
    (!stem.is_empty()).then_some(stem)
}

/// Bytes that are (incremental cache, stale artifacts). Every file lands in at
/// most one bucket, so the two are safe to add up — computing them separately
/// would double-count old files that live inside `incremental/`.
fn cleanup_buckets(files: &[Entry], base: &Path, now: SystemTime) -> (u64, u64) {
    let (mut incremental, mut old) = (0, 0);
    for f in files {
        let Ok(rel) = f.path.strip_prefix(base) else {
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

fn section(out: &mut String, title: &str, rows: &[(String, u64)]) {
    if rows.is_empty() {
        return;
    }
    line(out, "");
    line(out, title);
    line(out, RULE);
    line(out, "");
    let width = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    for (label, bytes) in rows {
        row_at(out, label, *bytes, width.max(LABEL_WIDTH));
    }
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
        let (incremental, old) = cleanup_buckets(&files, Path::new("/t"), now);
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
