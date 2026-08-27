use std::fs;
use std::path::PathBuf;

fn main() {
    let pier = required_payload("ARCEN_INSTALLER_PIER_EXE", "verified arcen-pier.exe");
    let cp = required_payload(
        "ARCEN_INSTALLER_CP_DLL",
        "matching arcen_credential_provider.dll",
    );
    println!("cargo:rerun-if-env-changed=ARCEN_INSTALLER_PIER_EXE");
    println!("cargo:rerun-if-env-changed=ARCEN_INSTALLER_CP_DLL");
    println!("cargo:rerun-if-changed={}", pier.display());
    println!("cargo:rerun-if-changed={}", cp.display());
    println!("cargo:rustc-env=ARCEN_EMBED_PIER_EXE={}", pier.display());
    println!("cargo:rustc-env=ARCEN_EMBED_CP_DLL={}", cp.display());
}

fn required_payload(variable: &str, description: &str) -> PathBuf {
    let value = std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "{variable} must name the {description}; build through hosts/windows/build.cmd or set the path explicitly"
            )
        });
    let path = PathBuf::from(value);
    let path = fs::canonicalize(&path)
        .unwrap_or_else(|error| panic!("resolve {variable} path {}: {error}", path.display()));
    assert!(
        path.is_file(),
        "{variable} must name a file, got {}",
        path.display()
    );
    path
}
