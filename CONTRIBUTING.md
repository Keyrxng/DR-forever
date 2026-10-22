# Contributing

Thanks for helping with `drf-server`.

## Before you start

- Read [`README.md`](README.md), [`NOTICE.md`](NOTICE.md), and
  [`PROVENANCE.md`](PROVENANCE.md).
- This is an unofficial interoperability project. Do not add game assets,
  client binaries, captures, extracted data, or code that is incompatible with
  GPLv3. Adapted code must be GPLv3-compatible and attributed.
- Keep changes focused. Small, reviewable pull requests are preferred.
- Do not add third-party Rust dependencies without discussing the need first.
  The server crates are intentionally `std`-only.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Content-dependent tests need a local client installation:

```sh
DRF_GAME_PKI=/path/to/game.pki \
DRF_GAME_PKG=/path/to/game.pkg \
cargo test --workspace
```

Never commit the paths or contents of those files.

## Code style

- Follow the existing Rust style and module boundaries.
- Prefer clear names and small functions over comments that restate the code.
- Comments should explain **why**, protocol constraints, or non-obvious
  invariants. Do not leave private notes, local paths, chat quotes, or
  investigation scratchpads in production code.
- Keep protocol comments factual and self-contained. A reader should not need
  access to a private lab repository to understand a comment.
- Avoid adding secrets, credentials, or machine-specific paths.
- New protocol behavior should have a test that pins the expected bytes or
  state transition.

## Commit and pull request guidance

- Describe the behavior changed and the verification performed.
- Include tests for bug fixes where practical.
- Call out any intentional compatibility deviation.
- Do not include unrelated formatting churn in a behavioral change.

## License

Contributions are accepted under the GNU General Public License v3.0 or later.
By opening a pull request, you confirm that you have the right to submit the
contribution under those terms. See [`PROVENANCE.md`](PROVENANCE.md) for the
full contribution policy.
