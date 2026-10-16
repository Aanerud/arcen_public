//! Smoke test: create a custom dispatch queue and exercise CG value types.
//!
//! Run with: `cargo run --example 02_dispatch_cg_smoke`

use apple_cf::cg::CGRect;
use apple_cf::dispatch_queue::{DispatchQoS, DispatchQueue};

fn main() {
    let q = DispatchQueue::new("dev.doom-fish.apple-cf.smoke", DispatchQoS::UserInitiated);
    println!("Created dispatch queue: {q:?}");
    println!("  raw ptr: {:p}", q.as_ptr());

    let r = CGRect::new(10.0, 20.0, 800.0, 600.0);
    let center = r.center();
    let size = r.size();
    let origin = r.origin();
    println!(
        "CGRect = ({}, {}, {} x {}); center = ({}, {})",
        origin.x, origin.y, size.width, size.height, center.x, center.y
    );
}
