# Building from Source

Most users should install from a prebuilt binary — see [Getting Started](getting-started.md).
Build from source if you want to modify inkentry, run the latest unreleased code, or
target a platform without a prebuilt release (Intel Macs included — no
`x86_64-apple-darwin` prebuilt is published).

## Prerequisites

### Rust

Install via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Rust 1.80 or later is required (inkentry uses the 2024 edition).

### No external inference server required

From v0.9.0, `inkentry-server` bundles the embedder
(codefuse-ai/F2LLM-v2-330M, 896-dim, via llama.cpp). No LM Studio, Ollama, or
other external inference server is needed. The CLI auto-starts the server on
first use; model weights are downloaded once, into the platform's own
local-data directory (see
[Where the model is cached](getting-started.md#where-the-model-is-cached)).

If you want GPU acceleration on macOS, build `inkentry-server` with the `llama-metal`
feature (see [Build feature flags](#build-feature-flags) below).

### Vulkan SDK (only for `llama-vulkan` builds)

The optional `llama-vulkan` server feature (cross-vendor GPU embedding on
Windows/Linux via llama.cpp) needs the [Vulkan SDK](https://vulkan.lunarg.com/)
at build time: `glslc` compiles the compute shaders, and the headers/loader are
linked against. On Ubuntu, `apt install glslc libvulkan-dev spirv-headers cmake`
is enough — `spirv-headers` supplies the `SPIRV-Headers` CMake package ggml's
`vulkan-shaders-gen` looks for, and the build fails at `find_package` without it.
Nothing Vulkan is required at *runtime*: the Vulkan backend is a runtime-loaded
module that fails to load, and degrades to CPU, on machines without a driver.
A default build never touches any of this. One runtime gotcha on Linux: the
process must be in the `render` group to open the DRM render node, or it
silently degrades to CPU even with a working driver — see "Linux GPU
acceleration and the `render` group" in [server-setup.md](server-setup.md).

## Build

This is a Cargo workspace with four crates: `inkentry-core` (library),
`inkentry-cli` (`inkentry` binary), `inkentry-embed` (embedding engines
library), and `inkentry-server` (`inkentry-server` binary).
Build them all together:

```bash
git clone https://github.com/inkentries/inkentry
cd inkentry

# Debug build (faster compile, slower runtime)
cargo build

# Release build (optimised — use this for day-to-day use)
cargo build --release
```

This produces both binaries under `target/release/`. Copy them to your `$PATH`:

```bash
cp target/release/inkentry target/release/inkentry-server ~/.local/bin/
# or
sudo cp target/release/inkentry target/release/inkentry-server /usr/local/bin/
```

Verify:

```bash
inkentry --version
inkentry-server --version
```

### Building individual binaries

```bash
# CLI only
cargo build --release -p inkentry-cli

# Server only
cargo build --release -p inkentry-server
```

## Build feature flags

### inkentry-server features

| Feature | Default | Description |
|---|---|---|
| `embed-llama` | yes | Bundle the F2LLM-v2-330M embedder (llama.cpp engine, CPU) and its Hugging Face Hub download path. Disabling it builds a server with no embedding capability at all: embed endpoints return a permanent 400 (there is no external-endpoint fallback). The device is selected at runtime (`INKENTRY_EMBED_DEVICE=auto\|gpu\|cpu`). |
| `llama-metal` | no | llama.cpp engine with Metal GPU acceleration on macOS — the macOS release target. Implies `embed-llama`. |
| `llama-vulkan` | no | llama.cpp engine with Vulkan + runtime-loaded backend modules — the cross-vendor Windows/Linux GPU target shipped in release binaries. Implies `embed-llama`. Needs the Vulkan SDK at build time (see Prerequisites); produces shared libraries and `ggml` modules that must ship next to the binary. |

Enable non-default features with `--features`:

```bash
# macOS release build with Metal GPU acceleration
cargo build --release -p inkentry-server --features llama-metal

# Windows/Linux build with the llama.cpp Vulkan engine (needs the Vulkan SDK)
cargo build --release -p inkentry-server --features llama-vulkan

# Server without the bundled embedder (no embedding capability at all)
cargo build --release -p inkentry-server --no-default-features
```

### inkentry-cli features

| Feature | Default | Description |
|---|---|---|
| `rich-formats` | yes | Parse PDF, DOCX, and XLSX files during indexing (pulls in `lopdf`, `docx-rs`, and `calamine`). Every published release binary includes it. |

```bash
# CLI without the PDF, DOCX and XLSX readers
cargo build --release -p inkentry-cli --no-default-features
```

### inkentry-core features

| Feature | Default | Description |
|---|---|---|
| `rich-formats` | no | The parsers themselves. Off in the library's own defaults; `inkentry-cli` turns it on through its default feature, so a standalone `inkentry-server` build does not pull `lopdf`, `docx-rs` or `calamine` in. |

## Running tests

```bash
cargo test
```

## Security audit

Requires [cargo-audit](https://crates.io/crates/cargo-audit):

```bash
cargo install cargo-audit
cargo audit
```

## Notes

- The `sqlite-vec` extension is bundled at compile time — no system SQLite extension needed.
- Tree-sitter grammars are compiled as part of the build. If you bump the `tree-sitter` core
  version, check that all `tree-sitter-*` grammar crates are compatible (see `Cargo.toml`).
- Release builds enable LTO and `codegen-units = 1` for a smaller, faster binary.
  Expect a longer compile on first release build.
- Shared dependency versions are declared in the workspace root `Cargo.toml` under
  `[workspace.dependencies]`. Bump versions there, not in each crate's `Cargo.toml`.
