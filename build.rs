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
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR is set by Cargo");
    let out_dir = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .expect("OUT_DIR is set by Cargo");

    for filename in ["text.wgsl", "rect.wgsl"] {
        let relative_path = format!("wgsl/{filename}");
        let source_path = manifest_dir.join(&relative_path);
        println!("cargo:rerun-if-changed={relative_path}");

        let source = fs::read_to_string(&source_path).unwrap_or_else(|err| {
            panic!("failed to read {}: {err}", source_path.display());
        });
        let minified = minify_wgsl(&source);
        let output_path = out_dir.join(filename);
        fs::write(&output_path, minified).unwrap_or_else(|err| {
            panic!("failed to write {}: {err}", output_path.display());
        });
    }
}

fn minify_wgsl(source: &str) -> String {
    let source = strip_wgsl_comments(source);
    let mut output = String::with_capacity(source.len());

    for line in source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !output.is_empty() {
            output.push('\n');
        }

        let mut pending_space = false;
        for character in line.chars() {
            if character.is_whitespace() {
                pending_space = true;
            } else {
                if pending_space && !output.ends_with('\n') {
                    output.push(' ');
                }
                output.push(character);
                pending_space = false;
            }
        }
    }

    output
}

fn strip_wgsl_comments(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut characters = source.chars().peekable();
    let mut line_comment = false;
    let mut block_depth = 0_u32;

    while let Some(character) = characters.next() {
        if line_comment {
            if character == '\n' {
                output.push(character);
                line_comment = false;
            }
            continue;
        }

        if block_depth > 0 {
            match (character, characters.peek()) {
                ('/', Some('*')) => {
                    characters.next();
                    block_depth += 1;
                }
                ('*', Some('/')) => {
                    characters.next();
                    block_depth -= 1;
                }
                _ => {}
            }
            continue;
        }

        match (character, characters.peek()) {
            ('/', Some('/')) => {
                characters.next();
                output.push(' ');
                line_comment = true;
            }
            ('/', Some('*')) => {
                characters.next();
                output.push(' ');
                block_depth = 1;
            }
            _ => output.push(character),
        }
    }

    assert_eq!(block_depth, 0, "unterminated WGSL block comment");
    output
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
                println!("cargo:rustc-link-arg-bin=alacritty={}", res_path.display());
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
