use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    compile_shaders();

    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    #[cfg(windows)]
    compile_windows_resource();
}

fn compile_shaders() {
    println!("cargo:rerun-if-changed=src/renderer/native/terminal.metal");
    println!("cargo:rerun-if-changed=src/renderer/native/terminal.hlsl");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    for (file, entries) in [
        (
            "src/renderer/native/text.wgsl",
            &[
                ("vs_bg", naga::ShaderStage::Vertex),
                ("vs_text", naga::ShaderStage::Vertex),
                ("fs_bg", naga::ShaderStage::Fragment),
                ("fs_text", naga::ShaderStage::Fragment),
            ][..],
        ),
        (
            "src/renderer/native/rect.wgsl",
            &[
                ("vs_main", naga::ShaderStage::Vertex),
                ("fs_main", naga::ShaderStage::Fragment),
            ][..],
        ),
    ] {
        println!("cargo:rerun-if-changed={file}");
        let source = fs::read_to_string(file).expect("read Vulkan shader source");
        let module = naga::front::wgsl::parse_str(&source).expect("parse Vulkan shader");
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("validate Vulkan shader");
        for (entry, stage) in entries {
            let pipeline = naga::back::spv::PipelineOptions {
                shader_stage: *stage,
                entry_point: (*entry).into(),
            };
            let words = naga::back::spv::write_vec(
                &module,
                &info,
                &naga::back::spv::Options::default(),
                Some(&pipeline),
            )
            .expect("compile Vulkan shader");
            let bytes: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
            fs::write(out_dir.join(format!("{entry}.spv")), bytes).expect("write Vulkan shader");
        }
    }
}

#[cfg(windows)]
fn compile_windows_resource() {
    println!("cargo:rerun-if-changed=windows/alacritty.rc");
    println!("cargo:rerun-if-changed=windows/alacritty.ico");

    let out_dir = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .expect("OUT_DIR is set by Cargo");
    let res_path = out_dir.join("alacritty.res");

    for compiler in ["rc.exe", "llvm-rc.exe", "llvm-rc"] {
        let status = Command::new(compiler)
            .current_dir("windows")
            .args(["/nologo", "/fo"])
            .arg(&res_path)
            .arg("alacritty.rc")
            .status();

        match status {
            Ok(status) if status.success() => {
                println!("cargo:rustc-link-arg-bin={}={}", env!("CARGO_PKG_NAME"), res_path.display());
                return;
            }
            Ok(status) => {
                println!("cargo:warning={compiler} failed with status {status}");
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                println!("cargo:warning=failed to run {compiler}: {err}");
            }
        }
    }

    panic!("failed to compile Windows resources; install rc.exe or llvm-rc");
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
