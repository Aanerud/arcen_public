use std::env;
use std::fs;
use std::io::{self, BufRead, Write};

const PROTOCOL_VERSION: u32 = 1;

fn json_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

fn capabilities_json() -> String {
    let os = env::consts::OS;
    let linux_uinput = os == "linux" && exists("/dev/uinput");
    let linux_uhid = os == "linux" && exists("/dev/uhid");
    format!(
        concat!(
            "{{",
            "\"type\":\"device_capabilities\",",
            "\"helper\":\"arcen-input-helper\",",
            "\"protocol_version\":{},",
            "\"status\":\"prototype\",",
            "\"platform\":\"{}\",",
            "\"semantic_input\":true,",
            "\"typed_hid\":true,",
            "\"linux_uinput\":{},",
            "\"linux_uhid\":{},",
            "\"hot_path\":\"rust-stdio\",",
            "\"backends\":[\"noop\",\"linux-uinput-prototype\",\"linux-uhid-prototype\"]",
            "}}"
        ),
        PROTOCOL_VERSION,
        os,
        json_bool(linux_uinput),
        json_bool(linux_uhid)
    )
}

fn write_line(line: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(line.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn run_stdio() -> io::Result<()> {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        match line.trim() {
            "" => {}
            "ping" => write_line("{\"type\":\"pong\"}")?,
            "capabilities" => write_line(&capabilities_json())?,
            "quit" => {
                write_line("{\"type\":\"ok\",\"message\":\"bye\"}")?;
                break;
            }
            other => write_line(&format!(
                "{{\"type\":\"device_error\",\"code\":\"UNKNOWN_COMMAND\",\"message\":\"{}\"}}",
                json_escape(other)
            ))?,
        }
    }
    Ok(())
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            c if c.is_control() => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

pub fn run_with_args(args: &[String]) -> io::Result<()> {
    if args.iter().any(|arg| arg == "--capabilities") {
        return write_line(&capabilities_json());
    }
    if args.iter().any(|arg| arg == "--stdio") {
        return run_stdio();
    }
    write_line("arcen-input-helper --capabilities | --stdio")
}

/// Entry point when this helper runs as its own binary.
pub fn run() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    run_with_args(&args)
}
