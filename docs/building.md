# Building and releasing

- [Building from source](#building-from-source)
- [System requirements](#system-requirements)
- [Tests](#tests)
- [Release builds](#release-builds)

## Building from source

Rusta needs Rust 1.88 or newer (edition 2024):

```bash
cargo build --release -p rusta-cli                  # target/release/rusta
cargo build --release -p rusta-cli --features tui   # with the terminal UI
```

## System requirements

The binary is self-contained: TLS is rustls, so there is no OpenSSL dependency,
and nothing else to install.

| Platform | Requirement |
| --- | --- |
| Linux x86_64 / aarch64 | glibc 2.17+ (Ubuntu 14.04+, Debian 8+, RHEL/CentOS 7+) for the portable builds below |
| macOS | macOS 11 Big Sur or newer, Apple Silicon (aarch64) and Intel (x86_64) |
| Windows | Windows 10 / 11, 64-bit (x86_64) |

The `zai_vision` MCP server, started for GLM sessions that need image
descriptions, needs Node.js 22 or newer for `npx`.

## Tests

```bash
cargo test --workspace
cargo test --workspace --features rusta-cli/tui
```

Some tests are ignored by default because they need something outside the
repository:

```bash
# The REPL and the TUI in a real pseudo-terminal against a mock model endpoint
# (needs python3).
cargo test -p rusta-cli --test repl_pty -- --ignored
cargo test -p rusta-cli --features tui --test tui_pty -- --ignored

# The live provider APIs, with KIMI_API_KEY, ZAI_API_KEY and DEEPSEEK_API_KEY
# from the environment or the repository's `.env` (see `.env.example`).
cargo test -p agent-core --test real_provider -- --ignored

# A full Devscriptor binary.
DEVSCRIPTOR_BIN=/path/to/devscriptor cargo test -p mcp-client --test devscriptor_integration -- --ignored
```

CI (`.github/workflows/ci.yml`) runs on every push to `main` and every pull request:
rustfmt; clippy and the tests, with and without the TUI, on each supported
platform — Linux x64 and ARM64, macOS on Apple Silicon and Intel, Windows x64;
and `cargo audit`. A failed clippy or test step is reported as annotations: each
compiler error, each failed test's output and the failed targets
(`.github/scripts/annotate-failures.py`).

## Release builds

The release workflow (`.github/workflows/release.yml`) builds every archive on
GitHub Actions, each on its own platform's runner, and checks each binary: it
reports the version in `Cargo.toml`, it needs glibc 2.17 at most on Linux and
macOS 11 on macOS, and it links no C++ runtime or OpenSSL, nor on Windows the
Visual C++ runtime. Pushing a tag `v<version>` builds the archives and publishes
them, with their checksums, as a GitHub release; the tag must match the version in
`Cargo.toml`. A manual run, or a push to `main` that changes the workflow, builds
and checks them without publishing them. The sections below build the same
archives by hand.

Rusta links no C++ runtime and no system library besides libc. Its only C code
is `ring`'s bundled crypto, which the target's C compiler builds (`zig cc`,
Apple's clang, MSVC). So the only portability concern on Linux is the **glibc
floor**, pinned with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild).
macOS and Windows are built natively. The release binary is the default
`rusta-cli` build; the TUI is opt-in and not part of it.

| Platform | Rust target triple | Build machine | Tool | Archive |
| --- | --- | --- | --- | --- |
| Linux x64 | `x86_64-unknown-linux-gnu` | x86_64 Linux | `cargo-zigbuild` (glibc 2.17) | `.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | ARM64 Linux, or x86_64 Linux (cross) | `cargo-zigbuild` (glibc 2.17) | `.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | an Apple Silicon Mac | `cargo build` | `.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | an Intel Mac, or cross from Apple Silicon | `cargo build` | `.tar.gz` |
| Windows x64 | `x86_64-pc-windows-msvc` | Windows with the MSVC build tools | `cargo build` | `.zip` |

Before building on any machine:

1. The version in `Cargo.toml` (`[workspace.package] version`) and `Cargo.lock`
   is the one being released, committed, and checked out on every build machine;
   otherwise `--locked` fails or the binary reports the wrong version.
2. Rust is a recent stable (`rustup update stable`).
3. Every archive holds a top-level directory `rusta-<version>-<target>/` with
   exactly one file, `rusta` (or `rusta.exe`).

Each bash block below runs as one subshell `( … )` from the repository root:
`set -e` pasted into an interactive shell would exit the shell itself on the
first failure.

### Linux (portable)

A plain `cargo build` links against the build machine's glibc, so a binary built
on a recent distribution refuses to start on older ones
(`version 'GLIBC_2.xx' not found`). `cargo zigbuild --target <triple>.2.17` pins
the floor to glibc 2.17. Install it once, with Zig 0.13 or newer on `PATH`:

```bash
cargo install cargo-zigbuild
```

Then, for `TARGET=x86_64-unknown-linux-gnu` — and again for
`TARGET=aarch64-unknown-linux-gnu`, which cargo-zigbuild cross-compiles from an
x86_64 host:

```bash
(
set -e
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
TARGET=x86_64-unknown-linux-gnu
NAME="rusta-${VERSION}-${TARGET}"

rustup target add "${TARGET}"
cargo zigbuild --release --locked --target "${TARGET}.2.17" -p rusta-cli

rm -rf "dist/${NAME}" && mkdir -p "dist/${NAME}"
cp "target/${TARGET}/release/rusta" "dist/${NAME}/rusta"
rm -f "dist/${NAME}.tar.gz"
tar -czf "dist/${NAME}.tar.gz" -C dist "${NAME}"
( cd dist && sha256sum "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256" )
rm -rf "dist/${NAME}"

# The glibc floor must be <= 2.17.
objdump -T "target/${TARGET}/release/rusta" | grep -oE 'GLIBC_[0-9.]+' | sort -V | tail -1
)
```

For x86_64, also check that no C++ runtime or OpenSSL is linked
(`ldd target/x86_64-unknown-linux-gnu/release/rusta | grep -cE 'stdc\+\+|ssl'`
prints `0`) and that the binary runs (`target/x86_64-unknown-linux-gnu/release/rusta version`).
An aarch64 binary cannot run on the x86_64 host; `file` confirms its
architecture.

### macOS

On an Apple Silicon Mac with the Xcode command-line tools
(`xcode-select --install`), for `TARGET=aarch64-apple-darwin` — and for
`TARGET=x86_64-apple-darwin`, which builds natively on an Intel Mac or
cross-compiles from Apple Silicon with no extra toolchain:

```bash
(
set -e
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
TARGET=aarch64-apple-darwin
NAME="rusta-${VERSION}-${TARGET}"
export MACOSX_DEPLOYMENT_TARGET=11.0   # the oldest macOS supported

rustup target add "${TARGET}"
cargo build --release --locked --target "${TARGET}" -p rusta-cli

rm -rf "dist/${NAME}" && mkdir -p "dist/${NAME}"
cp "target/${TARGET}/release/rusta" "dist/${NAME}/rusta"
rm -f "dist/${NAME}.tar.gz"
COPYFILE_DISABLE=1 tar -czf "dist/${NAME}.tar.gz" -C dist "${NAME}"   # no ._AppleDouble files
( cd dist && shasum -a 256 "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256" )
rm -rf "dist/${NAME}"
)
```

The binary is unsigned, so its first launch may need
`xattr -d com.apple.quarantine …`; codesign and notarize it before packaging if
you have an Apple Developer ID. The Intel binary also runs on Apple Silicon
through Rosetta 2.

### Windows

In PowerShell, with the Visual Studio C++ build tools (MSVC `link.exe`) and
rustup:

```powershell
$ErrorActionPreference = "Stop"
$VERSION = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
$TARGET = "x86_64-pc-windows-msvc"; $NAME = "rusta-$VERSION-$TARGET"

rustup target add $TARGET
$env:RUSTFLAGS = "-C target-feature=+crt-static"   # no Visual C++ redistributable needed
cargo build --release --locked --target $TARGET -p rusta-cli

Remove-Item -Recurse -Force "dist\$NAME" -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "dist\$NAME" | Out-Null
Copy-Item "target\$TARGET\release\rusta.exe" "dist\$NAME\rusta.exe"

Remove-Item -Force "dist\$NAME.zip" -ErrorAction SilentlyContinue
Compress-Archive -Path "dist\$NAME" -DestinationPath "dist\$NAME.zip"
(Get-FileHash -Algorithm SHA256 "dist\$NAME.zip").Hash.ToLower() + "  $NAME.zip" |
    Set-Content "dist\$NAME.zip.sha256"
Remove-Item -Recurse -Force "dist\$NAME"

& "target\$TARGET\release\rusta.exe" version
```

### After the builds

On any platform where the binary runs:

```bash
rusta version
rusta --provider kimi --api-key "$KIMI_API_KEY" doctor
```

`dist/` then holds five archives, each with its `.sha256`:

```text
rusta-<version>-x86_64-unknown-linux-gnu.tar.gz
rusta-<version>-aarch64-unknown-linux-gnu.tar.gz
rusta-<version>-aarch64-apple-darwin.tar.gz
rusta-<version>-x86_64-apple-darwin.tar.gz
rusta-<version>-x86_64-pc-windows-msvc.zip
```

Tag the release commit `v<version>`; pushing the tag runs the release workflow.
