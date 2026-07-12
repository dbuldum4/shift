# Shift

A minimal native macOS application built with [GPUI](https://www.gpui.rs/).

## Prerequisites

- macOS with Xcode and its command-line tools
- Rust stable (selected automatically by `rust-toolchain.toml`)

## Develop

```sh
cargo dev
```

The first build downloads and compiles GPUI and may take a few minutes.

## Checks

```sh
cargo fmt --check
cargo lint
cargo test
```

For an optimized binary, run `cargo build --release`. The executable will be at
`target/release/shift`.

GPUI is pre-1.0 and can introduce breaking changes. This starter pins GPUI to
`0.2.2`; update it deliberately and review its release notes when upgrading.
