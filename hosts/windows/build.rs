use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;

const SOURCE: &str = "data/cldr-48.2/windowsZones.xml";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    let mappings = parse_windows_zones(Path::new(SOURCE))
        .unwrap_or_else(|error| panic!("parse {SOURCE}: {error}"));
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("windows_zones.rs");
    let mut file = File::create(&output)
        .unwrap_or_else(|error| panic!("create {}: {error}", output.display()));
    writeln!(
        file,
        "pub(crate) static WINDOWS_ZONES: &[(&str, &str)] = &["
    )
    .expect("write generated header");
    for (iana, windows) in mappings {
        writeln!(file, "    ({iana:?}, {windows:?}),").expect("write generated mapping");
    }
    writeln!(file, "];").expect("write generated footer");
}

fn parse_windows_zones(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut reader = Reader::from_reader(BufReader::new(file));
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut mappings = BTreeMap::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Empty(element)) if element.name().as_ref() == b"mapZone" => {
                let mut windows = None;
                let mut iana = None;
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|error| error.to_string())?;
                    let value = attribute
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|error| error.to_string())?
                        .into_owned();
                    match attribute.key.as_ref() {
                        b"other" => windows = Some(value),
                        b"type" => iana = Some(value),
                        _ => {}
                    }
                }
                let windows = windows.ok_or_else(|| "mapZone missing other".to_string())?;
                let iana = iana.ok_or_else(|| "mapZone missing type".to_string())?;
                for identifier in iana.split_ascii_whitespace() {
                    match mappings.get(identifier) {
                        Some(existing) if existing != &windows => {
                            return Err(format!(
                                "conflicting Windows mappings for {identifier:?}: \
                                 {existing:?} and {windows:?}"
                            ));
                        }
                        Some(_) => {}
                        None => {
                            mappings.insert(identifier.to_string(), windows.clone());
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "XML error at byte {}: {error}",
                    reader.buffer_position()
                ));
            }
        }
        buffer.clear();
    }
    if mappings.is_empty() {
        return Err("no mapZone records found".to_string());
    }
    Ok(mappings)
}
