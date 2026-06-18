# Alacritty (wgpu 重构)

本仓库是 Alacritty 的 wgpu 重构版本, 目标是把渲染路径迁移到 wgpu 和 WGSL, 并整理为单一二进制 crate.

## 特性
- 基于 wgpu 的渲染, WGSL 着色器位于 `res/wgpu`
- 使用 `winit` 进行平台窗口与输入集成
- 单一二进制 crate 目录结构

## 目录结构
- `src/`: 应用, 终端核心与配置代码
- `res/`: 渲染资源与着色器
- `windows/`: Windows 资源文件
- `scripts/`: 辅助脚本

## 构建
在仓库根目录执行:

```
cargo build
```

## 运行
在仓库根目录执行:

```
cargo run
```

## 许可证
Apache-2.0
