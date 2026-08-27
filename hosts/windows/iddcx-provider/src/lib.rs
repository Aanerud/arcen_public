#![forbid(unsafe_code)]

pub mod abi;
mod edid;
mod model;

pub use edid::{EdidError, build_base_edid};
pub use model::{
    AdapterCandidate, AffinityError, CapabilityBlocker, CapabilityGate, MonitorSpec, TopologyError,
    TopologySpec, build_apply_request, evaluate_capabilities, resolve_render_adapter,
};
