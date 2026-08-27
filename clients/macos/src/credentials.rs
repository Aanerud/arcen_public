use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use thiserror::Error;
use zeroize::Zeroize;

pub const MAX_PASSWORD_FILE_SIZE: usize = 8 * 1024;
const MAX_CREDENTIALS_STDIN_SIZE: usize = (MAX_PASSWORD_FILE_SIZE * 2) + 4;

#[derive(Debug, Error)]
pub enum PasswordSourceError {
    #[error("--password and --password-file cannot be used together")]
    ConflictingSources,
    #[error("{0} requires a value")]
    MissingValue(&'static str),
    #[error("{0} may only be specified once")]
    DuplicateSource(&'static str),
    #[error("cannot inspect password file: {0}")]
    Inspect(#[source] io::Error),
    #[error("password file must not be a symbolic link")]
    Symlink,
    #[error("cannot open password file: {0}")]
    Open(#[source] io::Error),
    #[error("password file must be a regular file")]
    NotRegular,
    #[cfg(unix)]
    #[error("password file must be owned by the current user (owner uid {owner}, current uid {current})")]
    WrongOwner { owner: u32, current: u32 },
    #[cfg(unix)]
    #[error("password file permissions must be no broader than 0600 (found {mode:04o})")]
    InsecurePermissions { mode: u32 },
    #[error("password file changed while it was being opened")]
    ReplacedDuringOpen,
    #[error("cannot read password file: {0}")]
    Read(#[source] io::Error),
    #[error("password file exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("password file is empty")]
    Empty,
    #[error("password file contains a NUL byte")]
    ContainsNul,
    #[error("password file must contain valid UTF-8")]
    InvalidUtf8,
    #[error(
        "--credentials-stdin cannot be combined with --username, --password, or --password-file"
    )]
    CredentialsStdinConflict,
    #[error("--credentials-stdin may only be specified once")]
    DuplicateCredentialsStdin,
    #[error("cannot read credentials from stdin: {0}")]
    ReadCredentialsStdin(#[source] io::Error),
    #[error("credentials from stdin exceed the {limit}-byte limit")]
    CredentialsStdinTooLarge { limit: usize },
    #[error("credentials from stdin must contain exactly username and password on separate lines")]
    InvalidCredentialsStdinFormat,
    #[error("credentials from stdin contain a NUL byte")]
    CredentialsStdinContainsNul,
    #[error("credentials from stdin must contain valid UTF-8")]
    InvalidCredentialsStdinUtf8,
    #[error("username from stdin is empty")]
    EmptyUsername,
}

enum PasswordSource {
    Argument(String),
    File(PathBuf),
}

pub fn password_from_args(args: &[String]) -> Result<String, PasswordSourceError> {
    let mut password = None;
    let mut password_file = None;
    let mut index = 0;

    while index < args.len() {
        let flag = args[index].as_str();
        if flag != "--password" && flag != "--password-file" {
            index += 1;
            continue;
        }

        let value = args
            .get(index + 1)
            .cloned()
            .ok_or(PasswordSourceError::MissingValue(if flag == "--password" {
                "--password"
            } else {
                "--password-file"
            }))?;
        if flag == "--password" {
            if password.replace(value).is_some() {
                return Err(PasswordSourceError::DuplicateSource("--password"));
            }
        } else if password_file.replace(PathBuf::from(value)).is_some() {
            return Err(PasswordSourceError::DuplicateSource("--password-file"));
        }
        index += 2;
    }

    let source = match (password, password_file) {
        (Some(_), Some(_)) => return Err(PasswordSourceError::ConflictingSources),
        (Some(value), None) => Some(PasswordSource::Argument(value)),
        (None, Some(path)) => Some(PasswordSource::File(path)),
        (None, None) => None,
    };

    match source {
        Some(PasswordSource::Argument(password)) => Ok(password),
        Some(PasswordSource::File(path)) => read_password_file(&path),
        None => Ok(String::new()),
    }
}

pub fn credentials_from_args(args: &[String]) -> Result<(String, String), PasswordSourceError> {
    credentials_from_args_with_reader(args, std::io::stdin().lock())
}

fn credentials_from_args_with_reader(
    args: &[String],
    reader: impl Read,
) -> Result<(String, String), PasswordSourceError> {
    let stdin_count = args
        .iter()
        .filter(|arg| arg.as_str() == "--credentials-stdin")
        .count();
    if stdin_count > 1 {
        return Err(PasswordSourceError::DuplicateCredentialsStdin);
    }
    if stdin_count == 1 {
        if args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "--username" | "--password" | "--password-file"
            )
        }) {
            return Err(PasswordSourceError::CredentialsStdinConflict);
        }
        return read_credentials(reader);
    }

    Ok((
        value_from_args(args, "--username")?.unwrap_or_default(),
        password_from_args(args)?,
    ))
}

fn value_from_args(
    args: &[String],
    flag: &'static str,
) -> Result<Option<String>, PasswordSourceError> {
    let mut value = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] != flag {
            index += 1;
            continue;
        }
        let next = args
            .get(index + 1)
            .cloned()
            .ok_or(PasswordSourceError::MissingValue(flag))?;
        if value.replace(next).is_some() {
            return Err(PasswordSourceError::DuplicateSource(flag));
        }
        index += 2;
    }
    Ok(value)
}

fn read_credentials(reader: impl Read) -> Result<(String, String), PasswordSourceError> {
    let mut bytes = Vec::with_capacity(MAX_CREDENTIALS_STDIN_SIZE + 1);
    if let Err(source) = reader
        .take((MAX_CREDENTIALS_STDIN_SIZE + 1) as u64)
        .read_to_end(&mut bytes)
    {
        bytes.zeroize();
        return Err(PasswordSourceError::ReadCredentialsStdin(source));
    }
    if bytes.len() > MAX_CREDENTIALS_STDIN_SIZE {
        bytes.zeroize();
        return Err(PasswordSourceError::CredentialsStdinTooLarge {
            limit: MAX_CREDENTIALS_STDIN_SIZE,
        });
    }
    strip_one_line_ending(&mut bytes);
    if bytes.contains(&0) {
        bytes.zeroize();
        return Err(PasswordSourceError::CredentialsStdinContainsNul);
    }

    let mut text = String::from_utf8(bytes).map_err(|error| {
        let mut bytes = error.into_bytes();
        bytes.zeroize();
        PasswordSourceError::InvalidCredentialsStdinUtf8
    })?;
    let Some((username, password)) = text.split_once('\n') else {
        text.zeroize();
        return Err(PasswordSourceError::InvalidCredentialsStdinFormat);
    };
    let username = username.strip_suffix('\r').unwrap_or(username);
    if username.is_empty() {
        text.zeroize();
        return Err(PasswordSourceError::EmptyUsername);
    }
    if password.is_empty() || password.contains('\r') || password.contains('\n') {
        text.zeroize();
        return Err(PasswordSourceError::InvalidCredentialsStdinFormat);
    }

    let credentials = (username.to_string(), password.to_string());
    text.zeroize();
    Ok(credentials)
}

fn read_password_file(path: &Path) -> Result<String, PasswordSourceError> {
    let file = open_password_file(path)?;
    let mut bytes = Vec::with_capacity(MAX_PASSWORD_FILE_SIZE + 1);
    if let Err(source) = file
        .take((MAX_PASSWORD_FILE_SIZE + 1) as u64)
        .read_to_end(&mut bytes)
    {
        bytes.zeroize();
        return Err(PasswordSourceError::Read(source));
    }
    if bytes.len() > MAX_PASSWORD_FILE_SIZE {
        bytes.zeroize();
        return Err(PasswordSourceError::TooLarge {
            limit: MAX_PASSWORD_FILE_SIZE,
        });
    }

    strip_one_line_ending(&mut bytes);
    if bytes.is_empty() {
        return Err(PasswordSourceError::Empty);
    }
    if bytes.contains(&0) {
        bytes.zeroize();
        return Err(PasswordSourceError::ContainsNul);
    }

    String::from_utf8(bytes).map_err(|error| {
        let mut bytes = error.into_bytes();
        bytes.zeroize();
        PasswordSourceError::InvalidUtf8
    })
}

#[cfg(unix)]
fn open_password_file(path: &Path) -> Result<File, PasswordSourceError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let path_metadata = std::fs::symlink_metadata(path).map_err(PasswordSourceError::Inspect)?;
    if path_metadata.file_type().is_symlink() {
        return Err(PasswordSourceError::Symlink);
    }
    if !path_metadata.is_file() {
        return Err(PasswordSourceError::NotRegular);
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(PasswordSourceError::Open)?;
    let opened_metadata = file.metadata().map_err(PasswordSourceError::Inspect)?;
    if !opened_metadata.is_file() {
        return Err(PasswordSourceError::NotRegular);
    }
    if path_metadata.dev() != opened_metadata.dev() || path_metadata.ino() != opened_metadata.ino()
    {
        return Err(PasswordSourceError::ReplacedDuringOpen);
    }

    // SAFETY: geteuid takes no arguments and has no preconditions.
    let current_uid = unsafe { libc::geteuid() };
    if opened_metadata.uid() != current_uid {
        return Err(PasswordSourceError::WrongOwner {
            owner: opened_metadata.uid(),
            current: current_uid,
        });
    }
    let mode = opened_metadata.mode() & 0o777;
    if mode & !0o600 != 0 {
        return Err(PasswordSourceError::InsecurePermissions { mode });
    }

    Ok(file)
}

#[cfg(not(unix))]
fn open_password_file(path: &Path) -> Result<File, PasswordSourceError> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(PasswordSourceError::Open)?;
    if !file
        .metadata()
        .map_err(PasswordSourceError::Inspect)?
        .is_file()
    {
        return Err(PasswordSourceError::NotRegular);
    }
    Ok(file)
}

fn strip_one_line_ending(bytes: &mut Vec<u8>) {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    } else if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestFile {
        path: PathBuf,
    }

    impl TestFile {
        fn write(contents: &[u8]) -> Self {
            let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/credential-test-fixtures");
            std::fs::create_dir_all(&directory).expect("create password fixture directory");
            let path = directory.join(format!("arcen-password-file-{}-{id}", std::process::id()));
            std::fs::write(&path, contents).expect("write password test file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .expect("secure password test file");
            }
            Self { path }
        }

        fn args(&self) -> Vec<String> {
            vec![
                "arcen-client".to_string(),
                "--password-file".to_string(),
                self.path.display().to_string(),
            ]
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn reads_secure_file_and_strips_exactly_one_line_ending() {
        for (contents, expected) in [
            (&b"dummy-password\n"[..], "dummy-password"),
            (&b"dummy-password\r\n"[..], "dummy-password"),
            (&b"dummy-password\r"[..], "dummy-password"),
            (&b"dummy-password\n\n"[..], "dummy-password\n"),
        ] {
            let file = TestFile::write(contents);
            assert_eq!(password_from_args(&file.args()).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_both_password_sources() {
        let args = vec![
            "arcen-client".to_string(),
            "--password".to_string(),
            "dummy-password".to_string(),
            "--password-file".to_string(),
            "/unused".to_string(),
        ];
        assert!(matches!(
            password_from_args(&args),
            Err(PasswordSourceError::ConflictingSources)
        ));
    }

    #[test]
    fn reads_username_and_password_from_stdin_without_argument_values() {
        let args = vec![
            "arcen-client".to_string(),
            "media-smoke".to_string(),
            "host.example".to_string(),
            "--credentials-stdin".to_string(),
        ];
        assert_eq!(
            read_credentials(&b"automation\r\npassword-value\r\n"[..]).unwrap(),
            ("automation".to_string(), "password-value".to_string())
        );
        assert!(
            credentials_from_args_with_reader(&args, &b"automation\npassword-value\n"[..]).is_ok()
        );
    }

    #[test]
    fn rejects_credentials_stdin_with_argument_sources() {
        for flag in ["--username", "--password", "--password-file"] {
            let args = vec![
                "arcen-client".to_string(),
                "--credentials-stdin".to_string(),
                flag.to_string(),
                "unused".to_string(),
            ];
            assert!(matches!(
                credentials_from_args_with_reader(&args, &b"user\npassword\n"[..]),
                Err(PasswordSourceError::CredentialsStdinConflict)
            ));
        }
    }

    #[test]
    fn credentials_stdin_requires_exactly_two_nonempty_lines() {
        for contents in [
            &b"username-only\n"[..],
            &b"\npassword\n"[..],
            &b"username\n\n"[..],
            &b"username\npassword\nextra\n"[..],
        ] {
            assert!(read_credentials(contents).is_err());
        }
    }

    #[test]
    fn rejects_empty_file_after_line_ending() {
        let file = TestFile::write(b"\n");
        assert!(matches!(
            password_from_args(&file.args()),
            Err(PasswordSourceError::Empty)
        ));
    }

    #[test]
    fn rejects_nul_bytes() {
        let file = TestFile::write(b"dummy\0password");
        assert!(matches!(
            password_from_args(&file.args()),
            Err(PasswordSourceError::ContainsNul)
        ));
    }

    #[test]
    fn rejects_oversize_file() {
        let file = TestFile::write(&vec![b'x'; MAX_PASSWORD_FILE_SIZE + 1]);
        assert!(matches!(
            password_from_args(&file.args()),
            Err(PasswordSourceError::TooLarge { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_or_other_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let file = TestFile::write(b"dummy-password");
        std::fs::set_permissions(&file.path, std::fs::Permissions::from_mode(0o640))
            .expect("make password test file insecure");
        assert!(matches!(
            password_from_args(&file.args()),
            Err(PasswordSourceError::InsecurePermissions { mode: 0o640 })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let file = TestFile::write(b"dummy-password");
        let link = TestFile {
            path: file.path.with_extension("link"),
        };
        symlink(&file.path, &link.path).expect("create password test symlink");
        assert!(matches!(
            password_from_args(&link.args()),
            Err(PasswordSourceError::Symlink)
        ));
    }
}
