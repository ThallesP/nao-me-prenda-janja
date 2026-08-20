use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/index.html");
    println!("cargo:rerun-if-changed=web/package.json");
    println!("cargo:rerun-if-changed=web/vite.config.ts");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let web = Path::new(&manifest).join("web");
    let dist = web.join("dist");

    if std::env::var("SKIP_WEB_BUILD").is_ok() {
        if !dist.join("index.html").exists() {
            panic!("SKIP_WEB_BUILD set but web/dist/index.html missing");
        }
        return;
    }

    let bun = |args: &[&str]| {
        Command::new("bun")
            .args(args)
            .current_dir(&web)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    if !web.join("node_modules").exists() && !bun(&["install"]) {
        panic!("`bun install` failed in web/ — install bun or run it manually");
    }
    if !bun(&["run", "build"]) {
        panic!("`bun run build` failed in web/ — frontend must build before embedding");
    }
    if !dist.join("index.html").exists() {
        panic!("web/dist/index.html missing after build");
    }
}
