# Noema Studio

The **opt-in, good-looking** desktop UI for Noema Atlas — a Tauri v2 app whose
window hosts an HTML/CSS/Svelte front-end. The lean egui app (`noema-desktop`)
stays the default; Studio trades some RAM (a system webview — WKWebView on
macOS, WebView2 on Windows, WebKitGTK on Linux) for a modern look. It is **not**
Electron: there is no bundled Chromium.

It shares the exact same engine as the CLI and egui app: the Rust backend is a
thin layer of `#[tauri::command]`s over `noema-core` (no duplicated logic).

```
crates/studio/
  Cargo.toml            Rust crate (Tauri backend)
  build.rs              tauri-build
  tauri.conf.json       window + bundle config
  capabilities/         webview permissions (core:default)
  src/
    main.rs             opens the Engine, registers commands, runs Tauri
    commands.rs         search / detail / download / library / install / settings
    settings.rs         studio-settings.json (separate from egui's ui-settings.json)
  ui/                   Svelte + Vite front-end
    src/App.svelte      shell + sidebar + view router + live progress
    src/lib/api.js      invoke() wrappers + download event channel
    src/lib/views/*     Discover · Library · Transfers · Settings
```

## Toolchain

Studio is a **standalone workspace** (it has its own `Cargo.lock`) and is
`exclude`d from the repo-root workspace. Tauri 2.x's dependency tree
(`darling`, `serde_with`, `plist`, `time`) requires **rustc ≥ 1.88**, which is
newer than the lean crates' MSRV floor. Keeping Studio separate means the
default build's pinned dependencies are never perturbed by Tauri.

```bash
rustc --version   # must be >= 1.88 to build Studio; `rustup update` if not
```

## Develop

```bash
# one-time: install the front-end deps and the Tauri CLI
cd crates/studio/ui && npm install && cd -
cargo install tauri-cli --version "^2"   # or: npm i -g @tauri-apps/cli

# run the app (hot-reloads the Svelte UI, rebuilds Rust on change)
cd crates/studio && cargo tauri dev
```

> **Don't run the bare debug binary** (`target/debug/noema-studio`) on its own —
> a debug Tauri build is hard-wired to load the Vite dev server (`localhost:5173`),
> so without `cargo tauri dev` running it you'll get a **blank window**. To run
> the app standalone (embedded UI, no dev server), build release:
>
> ```bash
> cd crates/studio && cargo build --release   # → target/release/noema-studio
> ./target/release/noema-studio               # loads the bundled UI
> ```

`cargo tauri dev` runs `npm --prefix ui run dev` (Vite on :5173) and launches the
native window pointed at it.

## Build a release bundle

```bash
cd crates/studio && cargo tauri build
```

Produces a native installer/app for the current platform under
`target/release/bundle/`.

The committed `icons/icon.png` (an RGBA copy of `assets/logo.png`) is enough for
dev and `cargo build`. For polished distributable bundles, generate the full
per-platform icon set once with `cargo tauri icon icons/icon.png` (writes
`.icns`, `.ico`, and sized PNGs into `icons/`).

## What's wired

- **Discover** — live Hugging Face search → pick a GGUF quant or safetensors
  bundle → `download_model` imports the manifest and streams verified bytes.
- **Transfers** — live progress via the `download://progress` event channel,
  plus source-health stats.
- **Library** — installed/cached models, per-model share toggle, install-to-dir.
- **Settings** — persisted to `~/.noema/studio-settings.json`; speed cap,
  connections, share flags, mirror/proxy/tracker, theme.

Worldwide P2P seeding (the iroh feature) is intentionally **off** here to keep
the dependency footprint close to the CLI; the share toggles record intent and
the engine handles announce/seed where the feature is compiled in.
