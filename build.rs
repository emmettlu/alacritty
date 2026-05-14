use std::env;
use std::process::Command;
#[cfg(windows)]
use std::{
    fs,
    path::{Path, PathBuf},
};

fn main() {
    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    #[cfg(windows)]
    {
        copy_dxc_runtime();

        embed_resource::compile("./windows/alacritty.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}

#[cfg(windows)]
fn copy_dxc_runtime() {
    const DXC_DIR_ENV: &str = "ALACRITTY_DXC_DIR";

    println!("cargo:rerun-if-env-changed={DXC_DIR_ENV}");

    let Some(out_dir) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };

    let Some(profile_dir) = out_dir.ancestors().nth(3).map(Path::to_path_buf) else {
        return;
    };

    let Some(dxc_dir) = env::var_os(DXC_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    if !dxc_dir.is_dir() {
        return;
    }

    for dll in ["dxcompiler.dll", "dxil.dll"] {
        let src = dxc_dir.join(dll);
        let dst = profile_dir.join(dll);
        println!("cargo:rerun-if-changed={}", src.display());
        if let Err(err) = fs::copy(&src, &dst) {
            println!("cargo:warning=Failed to copy {} to {}: {err}", src.display(), dst.display());
        }
    }
}

fn commit_hash() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().into())
}
