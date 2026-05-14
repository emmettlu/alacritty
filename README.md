# Alacritty (wgpu restructure)

This repository is a restructured version of Alacritty with a wgpu based renderer. It keeps the terminal core and configuration crates while moving GPU rendering to wgpu and WGSL shaders.

## Highlights
- wgpu renderer with WGSL shaders in `alacritty/res/wgpu`
- Platform integration through `winit`
- Terminal core split into dedicated crates

## Repository layout
- `alacritty/`: application crate and entry point in `alacritty/src/main.rs`
- `alacritty_terminal/`: terminal core
- `alacritty_config/`: configuration types and parsing
- `alacritty_config_derive/`: derive macros for config
- `scripts/`: helper scripts

## Build
From the repository root:

```
cd alacritty
cargo build
```

## Run
From the repository root:

```
cd alacritty
cargo run
```

## License
Apache-2.0
