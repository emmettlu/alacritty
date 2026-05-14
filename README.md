# Alacritty (wgpu restructure)

This repository is a restructured version of Alacritty with a wgpu based renderer. It is organized as a single binary crate, with terminal core and configuration code inlined under `src/`.

## Highlights
- wgpu renderer with WGSL shaders in `res/wgpu`
- Platform integration through `winit`
- Single binary crate layout

## Repository layout
- `src/`: application, terminal core, and configuration code
- `res/`: renderer resources and shaders
- `windows/`: Windows resource files
- `scripts/`: helper scripts

## Build
From the repository root:

```
cargo build
```

## Run
From the repository root:

```
cargo run
```

## License
Apache-2.0
