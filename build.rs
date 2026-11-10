use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let manifest = root.join("DuoLink.manifest");
    println!("cargo:rerun-if-changed=DuoLink.manifest");
    println!("cargo:rerun-if-changed=assets/duolink.rc");
    println!("cargo:rerun-if-changed=assets/duolink.ico");
    println!("cargo:rustc-link-arg-bin=duolink=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=duolink=/MANIFESTINPUT:{}",
        manifest.display()
    );
    embed_resource::compile("assets/duolink.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
