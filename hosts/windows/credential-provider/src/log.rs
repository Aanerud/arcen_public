//! Secret-safe Credential Provider diagnostics.
//!
//! A credential provider has no stdout, no log file it may reliably write, and
//! no console. `OutputDebugStringW` is always available. An administrator may
//! additionally opt into a lab file by creating
//! `C:\ProgramData\Arcen\logs\enable-cp-diagnostics`; file diagnostics remain
//! disabled by default. Messages are deliberately terse and **never** contain
//! account names, secret material, keys, or serialized buffers.

/// Emit a diagnostic line, prefixed so it is greppable in a noisy debug stream.
#[cfg(windows)]
pub fn debug(message: &str) {
    use std::io::Write;
    use windows::core::PCWSTR;
    use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;

    let text = format!("[arcen-cp] {message}\r\n");
    let line: Vec<u16> = text.encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: `line` is a live NUL-terminated wide string for the duration of the call.
    unsafe { OutputDebugStringW(PCWSTR(line.as_ptr())) };

    let program_data = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
    let log_directory = program_data.join(r"Arcen\logs");
    let marker = log_directory.join("enable-cp-diagnostics");
    if marker.is_file() {
        let log = log_directory.join("credential-provider.log");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

/// Non-Windows stub so the pure build stays warning-clean.
#[cfg(not(windows))]
pub fn debug(_message: &str) {}
