//! Thin binary wrapper. All behaviour lives in the library so the Pier can
//! link it and expose it as a subcommand of the single `arcen-pier` binary.
fn main() {
    arcen_capenc::run();
}
