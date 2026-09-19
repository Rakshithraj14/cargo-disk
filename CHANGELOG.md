# Changelog

## 0.3.0

- `cargo disk deps` lists potentially unused dependencies with the space each
  would free, and direct dependencies that have a newer version. On a terminal
  it offers to remove the unused ones with `cargo remove`.
- `[package.metadata.cargo-disk] ignore = [...]` skips dependencies that text
  search cannot see being used.
- The target directory now comes from Cargo, so `build.target-dir` and
  `build.build-dir` in `.cargo/config.toml` are honored by the report, `clean`
  and `deps`. A separate build-dir is reported alongside target-dir.

## 0.2.0

- `cargo disk --all <dir>` ranks every Cargo project under a directory by
  `target/` size, with how long ago each was last built, plus the size of the
  shared `~/.cargo` caches.
- `cargo disk clean --incremental` deletes only incremental compilation caches,
  after listing them and asking. `--dry-run` lists without deleting, `--yes`
  skips the prompt. Nothing outside an `incremental` directory inside `target/`
  is ever touched, and symlinks are never followed.
- New report section: **Potential orphaned crates**, artifacts for crates the
  current dependency graph no longer mentions. Never counted as reclaimable.
- New report section: **Builds per crate**, shown only for crates with more than
  three build units. Counts only, deliberately without byte totals.
- `--help` and `--version`.
- Fixed: section columns widen to fit their longest label.

## 0.1.0

- First release: `target/` total, per-directory breakdown, largest crates, and
  potential cleanup for the current project.
