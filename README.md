# cargo-disk

Find out where your `target/` directory went.

Available on crates.io: [cargo-disk](https://crates.io/crates/cargo-disk)

```bash
cargo install cargo-disk
```

## Usage

```bash
cargo disk                      # this project
cargo disk --all ~/code         # every Cargo project under a directory
cargo disk clean --incremental  # delete only the incremental caches
```

## This project

`cargo disk` run on cargo-disk's own repository:

```text
Cargo Disk
────────────────────────────────────

Project: cargo-disk

Total disk usage
  target/               94.62 MB
  Cargo.lock               154 B

TARGET/
────────────────────────────────────

debug/                  93.14 MB
release/                 1.44 MB
package/                36.27 KB
.rustc_info.json         1.24 KB
CACHEDIR.TAG               177 B

DEBUG/
────────────────────────────────────

incremental/            68.30 MB
deps/                   24.84 MB
.fingerprint/            3.50 KB
cargo-disk.d               123 B
.cargo-artifact-lock         0 B
.cargo-build-lock            0 B
.cargo-lock                  0 B

Largest crates
────────────────────────────────────

cargo_disk                      94.58 MB
(other)                         37.68 KB

Builds per crate
────────────────────────────────────

cargo_disk                        9 builds

Potential cleanup
────────────────────────────────────

Incremental cache       68.30 MB

Potentially reclaimable:
                        68.30 MB
```

When `target/` holds artifacts for crates the current dependency graph no
longer mentions (say, a dependency you removed from `Cargo.toml`), a
**Potential orphaned crates** section lists them with a total. cargo-disk has
no dependencies, so it has none to show here.

## Every project on your machine

```text
$ cargo disk --all ~/GPU

Cargo Disk — Projects
────────────────────────────────────────

Project                   Target  Last used
cargo-disk              94.62 MB  3 minutes ago

Projects: 1
Combined target usage: 94.62 MB

Cargo home
────────────────────────────────────────

registry/              703.12 MB
Total shared cache:    703.12 MB
```

## Cleaning

`cargo clean` removes everything. This removes only the incremental caches,
which rustc rebuilds on demand:

```text
$ cargo disk clean --incremental --dry-run
68.30 MB   cargo-disk/target/debug/incremental

Dry run: nothing deleted (68.30 MB).
```

Without `--dry-run` it lists the same directories and asks before deleting.
`--yes` skips the prompt. It refuses to touch anything that is not an
`incremental` directory inside your `target/`, and never follows symlinks.

## Notes

- **Largest crates** adds up everything a crate left in `target/`: `.rlib`,
  `.rmeta`, `.d`, `build/`, `incremental/` and `.fingerprint/`, across all
  profiles. Files belonging to no crate are listed as `(other)`.
- **Potential orphaned crates** are artifacts for crates the current
  `cargo metadata` and `Cargo.lock` no longer mention. They are *not* counted as
  reclaimable: not being in the graph today is not proof Cargo will never use
  them. Removing them means `cargo clean`. The section is omitted when metadata
  cannot be resolved, rather than guessing.
- **Builds per crate** counts distinct build units, and is shown only above
  three. Several units per crate is normal — lib, test binary and `cargo check`
  metadata each get their own — so this is information, not waste.
- Hardlinked files are counted once, matching `du`. Cargo hardlinks
  `debug/<bin>` to `debug/deps/<bin>-<hash>`, and rustc hardlinks unchanged
  object files between incremental sessions; counting both overstates a
  `target/` by roughly 25%.
- Sizes are apparent file sizes in binary units (1 MB = 1024 KB), the same
  convention as `du -h`, not allocated blocks.
- `--all` skips hidden directories and `node_modules`, never descends into a
  `target/` it has found, and does not follow symlinks.
- Honors `CARGO_TARGET_DIR`. Does not yet honor `build.target-dir` from
  `.cargo/config.toml`.

## License

MIT
