# Contributing

Bug reports and pull requests are welcome. If a post stitches wrongly or
fails, include its URL and the command you ran.

## Layout

| Path | |
| --- | --- |
| `src/main.rs` | The whole tool, with its unit tests |
| `tests/cli.rs` | Tests that run the built binary |
| `docs/how-it-works.md` | The pipeline, rendering rules and limits |
| `release-please-config.json` | Release settings |

## Running from a checkout

```sh
cargo run -- https://x.com/user/status/1234567890 -o /tmp/x-stitch
```

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests need no network: nothing in them reaches X or the metadata
service. After changing how posts are resolved, downloaded or rendered, also
run the tool against a real post with several photos and one with a single
photo.

CI runs the same checks, plus `cargo check` on the minimum Rust version
declared in `Cargo.toml`.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/); release-please
builds the changelog and the release from them.

## Releases

release-please keeps a release pull request open on `main`. Merging it tags
the version, creates the GitHub release and publishes the crate to
crates.io.
