use std::path::PathBuf;

fn main() {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("DuoLink.manifest");
    println!("cargo:rerun-if-changed=DuoLink.manifest");
    println!("cargo:rustc-link-arg-bin=duolink=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=duolink=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
