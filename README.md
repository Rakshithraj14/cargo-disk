# cargo-disk

Find out where your `target/` directory went.

```bash
cargo install cargo-disk
```

```bash
cargo disk
```

```text
Cargo Disk
────────────────────────────────────

Project: clob-rs

Total disk usage
  target/               38.42 GB
  Cargo.lock               42 KB

TARGET/
────────────────────────────────────

debug/                  21.73 GB
release/                11.42 GB

DEBUG/
────────────────────────────────────

deps/                   13.82 GB
build/                   4.21 GB
incremental/             3.17 GB

Largest crates
────────────────────────────────────

sqlx_core                        1.82 GB
openssl_sys                      1.14 GB
tokio                          830.00 MB

Potential cleanup
────────────────────────────────────

Incremental cache        3.17 GB
Artifacts >30 days       5.92 GB

Potentially reclaimable:
                         9.09 GB
```

## Notes

- **Read-only.** It never deletes anything. `Potentially reclaimable` is an
  estimate to look at, not a promise that removing those bytes is safe.
- **Largest crates** adds up everything a crate left in `target/`: `.rlib`,
  `.rmeta`, `.d`, `build/`, `incremental/` and `.fingerprint/`, across all
  profiles. Files that belong to no crate are listed as `(other)`.
- Cleanup rows that are 0 are hidden.
- Hardlinked files are counted once, matching `du`. Cargo hardlinks
  `debug/<bin>` to `debug/deps/<bin>-<hash>`, and rustc hardlinks unchanged
  object files between incremental sessions; counting both overstates a
  `target/` by roughly 25%.
- Sizes are apparent file sizes, not allocated blocks, so a directory full of
  small files uses somewhat more disk than reported.
- Honors `CARGO_TARGET_DIR`. Does not yet honor `build.target-dir` from
  `.cargo/config.toml`.
- No options yet. `--json`, `--tree` and a `clean` subcommand are planned.

## License

MIT
