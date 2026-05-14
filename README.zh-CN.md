# Alacritty (wgpu 重构)

本仓库是 Alacritty 的 wgpu 重构版本, 目标是把渲染路径迁移到 wgpu 和 WGSL, 同时保留终端核心与配置相关的 crate.

## 特性
- 基于 wgpu 的渲染, WGSL 着色器位于 `alacritty/res/wgpu`
- 使用 `winit` 进行平台窗口与输入集成
- 终端核心与配置拆分为独立 crate

## 目录结构
- `alacritty/`: 应用 crate, 入口在 `alacritty/src/main.rs`
- `alacritty_terminal/`: 终端核心
- `alacritty_config/`: 配置类型与解析
- `alacritty_config_derive/`: 配置派生宏
- `scripts/`: 辅助脚本

## 构建
在仓库根目录执行:

```
cd alacritty
cargo build
```

## 运行
在仓库根目录执行:

```
cd alacritty
cargo run
```

## 许可证
Apache-2.0
