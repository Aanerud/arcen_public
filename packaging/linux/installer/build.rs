use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=ARCEN_PIER_BINARY");
    let Some(path) = std::env::var_os("ARCEN_PIER_BINARY") else {
        panic!("ARCEN_PIER_BINARY must point to the release arcen-pier binary to embed");
    };
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
            .join("../../..")
            .join(path)
    };
    if !absolute.is_file() {
        panic!(
            "ARCEN_PIER_BINARY does not name a file: {}",
            absolute.display()
        );
    }
    println!(
        "cargo:rustc-env=ARCEN_PIER_BINARY_ABS={}",
        absolute.display()
    );
    println!("cargo:rerun-if-changed={}", absolute.display());
}
