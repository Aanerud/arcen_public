use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_DIRECTORY: &str = "/etc/arcen";

pub fn main(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("new-host-cert: {error}");
            ExitCode::FAILURE
        }
    }
}

pub fn run(args: &[String]) -> Result<String, String> {
    let directory = parse_directory(args)?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    let cert = directory.join("host.crt");
    let key = directory.join("host.key");
    let force = args
        .iter()
        .any(|arg| arg == "--renew" || arg == "--new-key");
    if cert.exists() && key.exists() && !force {
        return Ok(format!(
            "TLS certificate already present at {} and {}",
            cert.display(),
            key.display()
        ));
    }
    let staged_key = key.with_file_name(format!(".host.key.installing.{}", std::process::id()));
    let staged_cert = cert.with_file_name(format!(".host.crt.installing.{}", std::process::id()));
    let key_for_cert = if args.iter().any(|arg| arg == "--renew") && key.exists() {
        key.clone()
    } else {
        let status = std::process::Command::new("openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
            ])
            .arg(&staged_key)
            .status()
            .map_err(|error| format!("start openssl key generation: {error}"))?;
        if !status.success() {
            return Err("openssl key generation failed".to_string());
        }
        chmod_600(&staged_key)?;
        staged_key.clone()
    };
    let status = std::process::Command::new("openssl")
        .args(["req", "-x509", "-new", "-sha256", "-days", "825"])
        .args(["-key"])
        .arg(&key_for_cert)
        .args(["-out"])
        .arg(&staged_cert)
        .args(["-subj", "/CN=Arcen Pier"])
        .args(["-addext", "basicConstraints=critical,CA:FALSE"])
        .args(["-addext", "keyUsage=critical,digitalSignature"])
        .args(["-addext", "extendedKeyUsage=serverAuth"])
        .status()
        .map_err(|error| format!("start openssl certificate generation: {error}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&staged_key);
        let _ = std::fs::remove_file(&staged_cert);
        return Err("openssl certificate generation failed".to_string());
    }
    if key_for_cert == staged_key {
        std::fs::rename(&staged_key, &key)
            .map_err(|error| format!("install {}: {error}", key.display()))?;
        chmod_600(&key)?;
    }
    std::fs::rename(&staged_cert, &cert)
        .map_err(|error| format!("install {}: {error}", cert.display()))?;
    chmod_600(&cert)?;
    Ok(format!(
        "generated TLS certificate at {} and key at {}",
        cert.display(),
        key.display()
    ))
}

fn parse_directory(args: &[String]) -> Result<PathBuf, String> {
    let mut directory = PathBuf::from(DEFAULT_DIRECTORY);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--directory" => {
                index += 1;
                directory = PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--directory requires a path".to_string())?,
                );
            }
            "--renew" | "--new-key" | "--adopt-legacy" => {}
            "--dns" | "--ip" => {
                index += 1;
                let _ = args
                    .get(index)
                    .ok_or_else(|| format!("{} requires a value", args[index - 1]))?;
            }
            "--help" | "-h" => {
                return Err(
                    "usage: arcen-pier new-host-cert [--renew|--new-key] [--directory DIR]"
                        .to_string(),
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(directory)
}

fn chmod_600(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    Ok(())
}
