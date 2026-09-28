use arcen_protocol::messages::BuildIdentityMsg;

#[must_use]
pub fn current() -> BuildIdentityMsg {
    arcen_protocol::build_identity::this_build("arcen-deck-macos", env!("CARGO_PKG_VERSION"))
}
