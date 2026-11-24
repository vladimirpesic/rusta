# Contributing to Rusta

Issues and pull requests are welcome. A security problem goes to the private
channels in [SECURITY.md](SECURITY.md), not to an issue.

## Before a pull request

[Building](docs/building.md) sets up the toolchain. A change is ready when these
pass, with no new warning, as CI runs them on Linux, macOS and Windows:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked --features rusta-cli/tui -- -D warnings
cargo test --workspace --locked
cargo test --workspace --locked --features rusta-cli/tui
```

[Building](docs/building.md#tests) lists the tests that are ignored by default:
the pseudo-terminal tests, the live provider tests and the Devscriptor
integration.

- **Docs follow the code.** A change that alters behaviour updates every document
  that describes it, in the same pull request.
- **Comments and docs describe the code as it is**: what it does and why, not how
  it came to be.

## Release notes

Each release's notes are its changelog; there is no other. They are
`.github/release-notes/v<version>.md`, written with the version bump, and the
release workflow publishes them as the release's description: a tag whose commit
has none builds nothing. They tell someone who uses Rusta what changed for them —
what is new, what is fixed, what to do on upgrading — not how the code changed.

## License

Rusta is licensed under the GPL-3.0-only. A contribution is licensed under the
same terms.
