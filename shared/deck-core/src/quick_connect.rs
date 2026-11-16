// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck window's quick-connect command line: open the window and connect
//! straight away, as the macOS Deck's `arcen-deck --connect HOST [PORT]`
//! does (PARITY HM-08).
//!
//! ```text
//! arcen-deck --connect HOST [PORT] [--username NAME] [--password-file PATH]
//!            [--pin-sha256 HEX] [--stream-stats] [--quit-after-secs N]
//! ```
//!
//! `PORT` defaults to the product's UDP port 18444. The password is never an
//! argument: it comes from a file the platform opens safely and
//! [`crate::password_text`] reads. `--stream-stats` opens Stream Statistics
//! with the session; `--quit-after-secs` ends the session and the window `N`
//! seconds after the desktop appears (unattended lab runs).

/// The product's QUIC port.
pub const DEFAULT_PORT: u16 = 18_444;

/// A parsed quick-connect request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickConnect {
    /// The Pier's host name or address.
    pub host: String,
    /// Its UDP port.
    pub port: u16,
    /// The account to sign in as (empty: ask, or let the host decide).
    pub username: String,
    /// Where the password is.
    pub password_file: Option<String>,
    /// The certificate's SHA-256 fingerprint to pin.
    pub pin_sha256: Option<String>,
    /// Open Stream Statistics with the session.
    pub stream_stats: bool,
    /// End the session and close the window this many seconds after the
    /// desktop appears.
    pub quit_after_secs: Option<u64>,
}

const VALUE_FLAGS: [&str; 4] = [
    "--username",
    "--password-file",
    "--pin-sha256",
    "--quit-after-secs",
];

/// Parses the window's arguments (without the program name).
///
/// # Errors
///
/// A missing or malformed value, an unknown flag, or a password given on
/// the command line.
pub fn parse_quick_connect(args: &[String]) -> Result<Option<QuickConnect>, String> {
    let Some(connect) = args.iter().position(|arg| arg == "--connect") else {
        return Ok(None);
    };
    let host = args
        .get(connect + 1)
        .filter(|value| !value.starts_with("--") && !value.is_empty())
        .cloned()
        .ok_or_else(|| "--connect requires a host".to_owned())?;
    let mut request = QuickConnect {
        host,
        port: DEFAULT_PORT,
        username: String::new(),
        password_file: None,
        pin_sha256: None,
        stream_stats: false,
        quit_after_secs: None,
    };
    let mut index = connect + 2;
    if let Some(port) = args.get(index).filter(|value| !value.starts_with("--")) {
        request.port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| format!("--connect port must be 1-65535, not {port}"))?;
        index += 1;
    }
    let mut rest: Vec<&String> = args[..connect].iter().collect();
    rest.extend(&args[index..]);
    let mut flags = rest.into_iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--stream-stats" => request.stream_stats = true,
            "--password" => {
                return Err("--password is not accepted here; use --password-file".to_owned());
            }
            name if VALUE_FLAGS.contains(&name) => {
                let value = flags
                    .next()
                    .filter(|value| !value.starts_with("--"))
                    .ok_or_else(|| format!("{name} requires a value"))?
                    .clone();
                match name {
                    "--username" => request.username = value,
                    "--password-file" => request.password_file = Some(value),
                    "--pin-sha256" => request.pin_sha256 = Some(value),
                    _ => {
                        request.quit_after_secs = Some(value.parse::<u64>().map_err(|_| {
                            format!("--quit-after-secs must be whole seconds, not {value}")
                        })?);
                    }
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Some(request))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Option<QuickConnect>, String> {
        let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        parse_quick_connect(&args)
    }

    #[test]
    fn no_connect_flag_opens_the_launcher() {
        assert_eq!(parse(""), Ok(None));
    }

    #[test]
    fn the_port_defaults_to_the_product_port_and_flags_go_anywhere() {
        let request = parse("--stream-stats --connect pier.example.internal --username ada")
            .unwrap()
            .unwrap();
        assert_eq!(request.host, "pier.example.internal");
        assert_eq!(request.port, DEFAULT_PORT);
        assert_eq!(request.username, "ada");
        assert!(request.stream_stats);

        let request = parse(
            "--connect 203.0.113.5 18555 --password-file C:\\pw.txt --pin-sha256 ab \
             --quit-after-secs 90",
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.port, 18_555);
        assert_eq!(request.password_file.as_deref(), Some("C:\\pw.txt"));
        assert_eq!(request.pin_sha256.as_deref(), Some("ab"));
        assert_eq!(request.quit_after_secs, Some(90));
    }

    #[test]
    fn bad_values_unknown_flags_and_argv_passwords_are_refused() {
        assert!(parse("--connect").is_err());
        assert!(parse("--connect --username ada").is_err());
        assert!(parse("--connect host 0").is_err());
        assert!(parse("--connect host --username").is_err());
        assert!(parse("--connect host --quit-after-secs soon").is_err());
        assert!(parse("--connect host --password hunter2").is_err());
        assert!(parse("--connect host --fullscreen").is_err());
    }
}
