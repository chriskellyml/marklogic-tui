fn main() {
    // Frontend changes must invalidate the executable containing Tauri's embedded assets.
    println!("cargo:rerun-if-changed=../dist");
    println!("cargo:rerun-if-changed=src");
    let built = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    println!("cargo:rustc-env=WORKBENCH_BUILD={built}");
    tauri_build::build()
}
