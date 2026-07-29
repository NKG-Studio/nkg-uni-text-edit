fn main() {
    println!("cargo:rerun-if-changed=assets/windows.rc");
    println!("cargo:rerun-if-changed=assets/nkg-icon.ico");

    embed_resource::compile("assets/windows.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed the Windows application icon");
}
