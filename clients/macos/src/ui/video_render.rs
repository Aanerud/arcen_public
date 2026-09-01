//! Primary remote-video surface: a `wgpu` paint callback that renders the
//! decoded video directly, replacing the CPU "decode -> BGRA -> RGBA
//! `Vec<u8>` -> `egui::ColorImage` -> `Context::load_texture`" path that used
//! to flatten every frame to 8-bit RGBA before it ever reached the screen
//! (previously `ArcenApp::update_remote_texture` around `app.rs:10956`, and
//! the `ui.painter().image(texture.id(), ...)` draw sites in
//! `viewer_input_surface`/`reconnect_screen`).
//!
//! # Scope: primary/root viewport only
//!
//! This replaces the **primary/root** monitor's video surface only
//! (`ArcenApp::remote_texture`). The secondary per-monitor multi-window
//! viewport path (`ArcenApp::secondary_textures`, driven from
//! `drive_multi_window`) deliberately keeps the old
//! `egui::TextureHandle`/`ui.painter().image(...)` mechanism: several
//! existing regression tests (`shapes_reference_texture`/
//! `shape_references_texture` in `app.rs`'s test module) hard-assert that a
//! secondary's remote frame paints as an `egui::Shape::Mesh` referencing a
//! specific `egui::TextureId` -- `ui.painter().image(...)`'s documented,
//! exact output shape. A `Callback`-based paint callback produces
//! `egui::Shape::Callback` instead, which carries no `TextureId` at all, so
//! switching the secondary path the same way would silently invalidate that
//! exact-detection contract in a way that cannot be re-verified here (this
//! change is not compile- or test-run-verified at all -- see the crate-level
//! note below). Converting the secondary path is therefore left as a
//! follow-up that also updates/rewrites those tests deliberately, rather
//! than done blind alongside this change.
//!
//! # What is real today vs. what is a seam
//!
//! [`RawVideoPayload::PackedRgba8`] is the only variant any production code
//! path constructs: `video_decoder.rs` (read-only for this change) still
//! only ever hands back a [`crate::pipeline::video_decoder::DecodedVideoFrame`]
//! whose `rgba` field is an *already-converted*, already-8-bit interleaved
//! RGBA buffer (VideoToolbox's `VTPixelTransferSession` does the BGRA
//! conversion on the CPU before the frame ever reaches this module -- see
//! `copy_biplanar_to_rgba`/`copy_locked_pixels` there). This module uploads
//! that buffer as a single `Rgba8Uint` "plane" and selects the shader's
//! passthrough mode (`ShaderMode::PackedRgba`): no matrix, no bit-depth
//! recovery, because there is no higher-fidelity data to recover it from.
//! That upload is a real, unconditional replacement of the old CPU
//! `ColorImage` path -- there is no dual path here, and no feature flag.
//!
//! [`RawVideoPayload::Planar16`], the shader's matrix and identity/GBR
//! branches, and [`PlaneBuffer`] are fully implemented and unit-tested
//! (construction, uniform derivation, mode selection) but are **not yet
//! reachable from any production code path**, because nothing in this
//! change can construct one: `video_decoder.rs` never hands back either a
//! `CVPixelBuffer` or raw plane bytes today, and this change may not edit
//! that file. This is the "clearly-marked trait/shim" the wiring depends on;
//! see the next section for exactly what `video_decoder.rs` needs to grow.
//!
//! # The exact `video_decoder.rs` change this depends on
//!
//! `video_decoder.rs`'s `DecodedVideoFrame` (and the whole
//! `copy_biplanar_to_rgba`/`VTPixelTransferSession` step that produces its
//! `rgba` field) would need to grow an alternative that hands back, per
//! decoded frame:
//!
//!  1. The negotiated `chroma`/`range`/`depth`/`matrix` actually decoded
//!     this frame (today this information is read inside
//!     `PlatformVideoDecoder::decode`/`rebuild_session_if_ready` purely to
//!     pick a `CVPixelBuffer` format and stamp `CMFormatDescription`
//!     extensions -- it is never threaded back out to the caller at all).
//!     [`VideoColorContract`] in this module is exactly that shape; a
//!     `From<arcen_media::VideoConfiguration>` conversion already exists
//!     here for it, so this may not even need a new type on that side.
//!  2. Either:
//!     - **Zero-copy (preferred):** the raw `CVPixelBuffer` (or an opaque
//!       handle wrapping it plus a `CVMetalTextureCache` reference), so this
//!       module can hand its planes to Metal directly via
//!       `CVMetalTextureCacheCreateTextureFromImage` with no CPU copy at
//!       all; or
//!     - **CPU fallback:** the locked luma/chroma plane bytes at up to
//!       16 bits per sample, MSB-aligned exactly like
//!       `ColorTransform::pack_p16`/`unpack_p16` (i.e. what
//!       `copy_biplanar_to_rgba` already has locked via
//!       `guard.plane_data(0)`/`guard.plane_data(1)` today, just not copied
//!       out as `u16`s instead of being handed to `VTPixelTransferSession`).
//!       [`PlaneBuffer::new`] is exactly the shape needed for this.
//!
//! Whichever is chosen, the mechanical wiring on this side is: construct a
//! [`RawVideoPayload::Planar16`] (CPU fallback) instead of `PackedRgba8` in
//! [`RemoteVideoFrame::from_decoded`] (or the zero-copy variant described
//! below), carrying the [`VideoColorContract`] through from (1).
//!
//! # Zero-copy Metal (`CVMetalTextureCache` -> `wgpu::Texture`): verified, but blocked
//!
//! `wgpu::Device::as_hal::<wgpu::hal::api::Metal>()` and
//! `wgpu::Device::create_texture_from_hal::<wgpu::hal::api::Metal>(...)`
//! **do exist** in the exact pinned versions (`wgpu`/`wgpu-hal` 29.0.4, read
//! directly from the vendored registry source, not guessed): both are
//! `#[cfg(wgpu_core)]`-gated, and `wgpu_core` is unconditionally enabled on
//! every native (non-wasm) target regardless of which backend feature is
//! active. `wgpu::hal::api::Metal` (`wgpu_hal::metal::Api`) is likewise real,
//! gated only on the `metal` cfg alias, which is on by default on
//! `target_vendor = "apple"` because `wgpu`'s own default feature set
//! includes `"metal"` and `eframe`'s `wgpu` feature turns on
//! `egui-wgpu/default`, which turns on `wgpu/default` -- so this is
//! reachable through ordinary Cargo feature unification with the
//! dependencies this crate already has, no new feature flags required.
//! `wgpu_hal::metal::Device` additionally has a real, public
//! `pub unsafe fn texture_from_raw(raw: Retained<ProtocolObject<dyn
//! MTLTexture>>, format: wgt::TextureFormat, raw_type: MTLTextureType,
//! array_layers: u32, mip_levels: u32, copy_size: CopyExtent) ->
//! wgpu_hal::metal::Texture` (`wgpu-hal-29.0.4/src/metal/device.rs`) --
//! exactly the wrapper `create_texture_from_hal` needs, and
//! `apple-cf` 0.9.3 (already a dependency) even has the raw FFI signature
//! for `CVMetalTextureCacheCreateTextureFromImage` and
//! `CVMetalTextureGetTexture` already declared (`apple_cf::raw::extras`),
//! just not wrapped.
//!
//! What blocks it: `texture_from_raw`'s `Retained<ProtocolObject<dyn
//! MTLTexture>>` parameter requires naming the `MTLTexture` protocol trait,
//! which is defined in the `objc2-metal` crate -- a *transitive* dependency
//! of this crate today (pulled in only by `wgpu-hal`'s own Cargo.toml, with
//! its own curated feature list), not a direct one. Rust does not let a
//! crate name a type from a dependency it does not itself declare, and
//! nothing in `wgpu`/`wgpu-hal`'s public API re-exports the `objc2-metal`
//! crate or its `MTLTexture` trait under any path this crate can already
//! reach. The `CAMetalLayer` colourspace piece of w4-10bit-drawable has the
//! identical shape of blocker, via `objc2-quartz-core`. Per this task's own
//! constraints this crate's `Cargo.toml` may not be edited, so the concrete,
//! minimal unblock -- reported, not applied -- is adding, as direct
//! dependencies of `arcen-deck-macos`, exactly the versions/features
//! `wgpu-hal` 29.0.4 itself already builds against (so no version skew is
//! introduced):
//!
//! ```toml
//! [target.'cfg(target_os = "macos")'.dependencies]
//! objc2-metal = { version = "0.3.2", default-features = false, features = [
//!     "std", "MTLDevice", "MTLTexture", "MTLPixelFormat", "MTLTypes",
//! ] }
//! objc2-quartz-core = { version = "0.3.2", default-features = false, features = [
//!     "std", "CALayer", "CAMetalLayer", "objc2-metal",
//! ] }
//! ```
//!
//! Given that, this module falls back to the CPU-side upload path the task
//! allows for exactly this situation (`RawVideoPayload::Planar16`, at up to
//! 16 bits/sample) rather than pretending the zero-copy path is wired up.
//!
//! # The swapchain itself stays 8-bit: also verified, also blocked
//!
//! w4-10bit-drawable asks for *the* presentation surface at
//! `Rgb10a2Unorm`. Reading `egui-wgpu` 0.35.0's own source
//! (`egui-wgpu-0.35.0/src/lib.rs::RenderState::create` and
//! `::preferred_framebuffer_format`) shows it unconditionally computes
//! `target_format` by scanning the surface's supported formats for
//! `Rgba8Unorm`/`Bgra8Unorm` and picking the first match, with **no
//! configuration hook** in `egui_wgpu::WgpuConfiguration`/
//! `egui_wgpu::SurfaceConfig` to override it -- and `wgpu_hal::metal`'s own
//! `surface_capabilities()` (`wgpu-hal-29.0.4/src/metal/adapter.rs`) always
//! lists `Bgra8Unorm` first, so on every Mac this resolves deterministically
//! to `Bgra8Unorm`, decided once, before any of `ArcenApp`'s own code runs,
//! and baked into the `Renderer`'s own pipelines for the rest of `egui`'s
//! chrome. Changing it after the fact from application code would desync
//! egui's own render pipelines from the surface and break the rest of the
//! UI, not just the video surface. This is a real architectural fact of
//! `eframe`/`egui-wgpu` 0.35, not a missing dependency, and there is no seam
//! reachable from `app.rs`/this module to fix it -- it would need either an
//! upstream `egui-wgpu` capability or bypassing `eframe`'s own window/surface
//! management for this one surface (a much larger change, and not something
//! `app.rs`-level code can do). This remains true after the "Unblocked
//! 2026-08-14" section below: only the layer's *colour space*, never its
//! pixel *format*, turned out to be reachable and safe to change -- see
//! that section for exactly why the two are different.
//!
//! What *is* real and delivered here: the actual YCbCr/GBR -> RGB conversion
//! (`fs_main` in `video_render.wgsl`) resolves into a genuine
//! `wgpu::TextureFormat::Rgb10a2Unorm` render target at the source's native
//! resolution -- verified against `wgpu-types-29.0.4`'s own format
//! capability table to require no extra `wgpu::Features` and to be a native
//! `RENDER_ATTACHMENT` + filterable-`Float`-sample-type format (mapped by
//! `wgpu-hal`'s Metal backend straight to `MTLPixelFormatRGB10A2Unorm`; see
//! `wgpu-hal-29.0.4/src/metal/adapter.rs`). That target is the SDR 10-bit
//! reference-viewing surface this task asks for; it is just an internal
//! `wgpu` render target rather than the `CAMetalLayer`-backed presentation
//! surface, for the reasons above. A second, trivial bilinear-resolve pass
//! (`fs_composite`) then samples it into whatever `egui`'s own render pass
//! actually is.
//!
//! # Re-verified 2026-08-14, plus the `raw-window-handle` alternative
//!
//! A later pass on this same task re-checked every claim in the two
//! "verified, but blocked" sections above directly against the vendored
//! registry sources on the build machine (not re-guessed): `Cargo.lock`
//! confirms `objc2-metal 0.3.2`/`objc2-quartz-core 0.3.2` are pulled in
//! *only* by `wgpu-hal 29.0.4`'s own dependency list, not as direct
//! dependencies of this crate; `wgpu-hal-29.0.4/src/metal/device.rs`'s
//! `texture_from_raw` signature, `wgpu-hal-29.0.4/src/metal/adapter.rs`'s
//! `surface_capabilities()` (`Bgra8Unorm` first, `Rgb10a2Unorm` only
//! conditionally appended after), and `egui-wgpu-0.35.0/src/lib.rs`'s
//! `preferred_framebuffer_format`/`SurfaceConfig` (no format field at all)
//! all match what is written above verbatim.
//!
//! This pass also checked the specific alternative this task's own
//! description names -- reaching the `CAMetalLayer` via `raw-window-handle`
//! instead of `wgpu_hal::metal` -- and it is blocked by the identical shape
//! of problem, not a different one:
//!
//!  1. `wgpu-hal-29.0.4/src/metal/mod.rs`'s real `Instance::create_surface`
//!     shows this is exactly how `wgpu-hal` itself gets the layer: it
//!     matches `raw_window_handle::RawWindowHandle::AppKit(handle)` and
//!     calls `raw_window_metal::Layer::from_ns_view(handle.ns_view)`. The
//!     same raw `NSView` pointer is, in principle, reachable from
//!     application code too -- `eframe::Frame`/`epi::CreationContext`
//!     genuinely implement `raw_window_handle::HasWindowHandle`
//!     (`eframe-0.35.0/src/epi.rs`) -- but `raw-window-handle` reaches this
//!     crate today only transitively (through `winit`/`wgpu`/`eframe`
//!     themselves), and `eframe` re-exports neither the `raw_window_handle`
//!     crate nor its `HasWindowHandle` trait under any path (`epi.rs`'s own
//!     `use raw_window_handle::{...}` is a private `use`, not `pub use`).
//!     Naming `HasWindowHandle` (even just to call `.window_handle()`, or to
//!     match on the `RawWindowHandle::AppKit` variant) requires
//!     `raw-window-handle` as a *direct* Cargo.toml dependency of this
//!     crate -- the same Cargo-level blocker as `objc2-metal`/
//!     `objc2-quartz-core` above, just for a third crate.
//!  2. The obvious workaround -- read the `NSView` straight off
//!     `NSApplication`'s own window via the `objc2-app-kit` this crate
//!     already depends on directly, skipping `raw-window-handle` entirely
//!     -- is *also* blocked, and for the same class of reason: this crate's
//!     `objc2-app-kit` dependency enables the `"NSWindow"` feature but not
//!     `"NSView"`, and `objc2-app-kit-0.3.2/src/generated/NSWindow.rs`'s own
//!     `contentView()` accessor is `#[cfg(feature = "NSView")]`-gated, so it
//!     does not exist in this build at all. Even holding an `NSView` this
//!     way, its `CALayer`/`CAMetalLayer` would still need
//!     `objc2-quartz-core` (or raw `objc2::msg_send!` reconstructing every
//!     selector/argument type by hand) to do anything with.
//!
//! Every one of these three independent avenues -- `wgpu_hal::metal`,
//! `raw-window-handle`, and `objc2-app-kit`'s own `NSWindow` -- dead-ends at
//! the same Cargo.toml wall: a direct dependency (or an additional Cargo
//! feature on an existing one) this task's constraints do not permit adding.
//! `w4-10bit-drawable` is left `blocked` on that specific, re-confirmed
//! evidence rather than attempted further.
//!
//! # Unblocked 2026-08-14: the `CAMetalLayer`'s colour space, not its format
//!
//! A follow-up pass started from a Cargo change made just before it, by the
//! task's owner rather than by this pass: `clients/macos/Cargo.toml`'s
//! `objc2-app-kit` dependency now enables the `"NSView"` feature it was
//! missing above (alongside the `"NSWindow"` it already had), with no new
//! crate and no `Cargo.lock` change (verified). Re-reading
//! `objc2-app-kit-0.3.2/src/generated/NSWindow.rs` confirms this is exactly
//! what bullet 2 above needed: `contentView()`'s
//! `#[cfg(feature = "NSView")]` gate is now satisfied, so
//! `NSWindow::contentView() -> Option<Retained<NSView>>` is real in this
//! build.
//!
//! That alone still does **not** reach `CALayer`/`CAMetalLayer`, and this
//! pass re-confirmed exactly why before writing anything against it:
//! `objc2-app-kit-0.3.2/src/generated/NSView.rs`'s *typed* `layer()`/
//! `setLayer()` accessors are gated `#[cfg(feature = "objc2-quartz-core")]`
//! -- a *different*, still-disabled feature of `objc2-app-kit` itself (its
//! own `Cargo.toml` shows `"NSView" = ["bitflags", "objc2-foundation/...",
//! ...]`, never `"objc2-quartz-core"`) -- so those two methods still do not
//! exist in this build, and naming `CALayer` any other typed way still
//! needs `objc2-quartz-core` as a direct dependency, exactly as bullet 2
//! above found.
//!
//! The actual unblock is a technique, not a new dependency: **raw
//! `objc2::msg_send!` never needs to name `CALayer`/`CAMetalLayer` as a Rust
//! type at all**, the same way bullet 2 above already noted in passing ("or
//! raw `objc2::msg_send!` reconstructing every selector/argument type by
//! hand") without following through on it. This is precisely what
//! `raw-window-metal` 1.1.0 -- the crate `wgpu-hal`'s own
//! `Instance::create_surface` uses to create this exact layer, cited above
//! -- does internally, and says so in its own doc comment
//! (`raw-window-metal-1.1.0/src/lib.rs`): *"We use `NSObject` here to avoid
//! importing `objc2-app-kit`"*, immediately before its own
//! `let root_layer: Option<Retained<CALayer>> = unsafe { msg_send![ns_view,
//! layer] };`. Applying the same trick one level further -- `NSObject`
//! standing in for the not-yet-typed `CALayer` return, exactly like it
//! stands in for `NSView` in their code -- reaches the layer with zero new
//! dependencies:
//!
//!  1. `NSWindow::contentView()` (now real, see above) gives a typed
//!     `Retained<NSView>` via an ordinary, safe `objc2-app-kit` call.
//!  2. `unsafe { msg_send![&*view, layer] }`, typed as
//!     `Option<Retained<NSObject>>`, sends the exact same `-[NSView layer]`
//!     selector `raw-window-metal`'s own code sends, just spelled with
//!     `NSObject` instead of `CALayer` as the stand-in return type -- valid
//!     because an Objective-C message send only needs a receiver and a
//!     selector at the ABI level; the Rust-side return-type annotation is
//!     purely this crate's own bookkeeping, not something the runtime
//!     checks.
//!  3. The returned layer is the view's *root* layer, which is **not**
//!     generally the `CAMetalLayer` `wgpu` actually presents through.
//!     `raw-window-metal-1.1.0/src/lib.rs`'s own module doc explains why:
//!     *"If a view does not have a `CAMetalLayer` as the root layer (as is
//!     the default for most views) ... [option 3] Create a sublayer ...
//!     This is what this crate does."* -- and `winit-0.30.13`'s own AppKit
//!     view source (`src/platform_impl/macos`) never overrides
//!     `layerClass` or mentions `CAMetalLayer` anywhere, confirming
//!     `eframe`'s `winit` window is one of those default, non-Metal-rooted
//!     views. So `wgpu-hal`'s Metal surface is genuinely a *sublayer* here,
//!     found by walking `-[CALayer sublayers]` (again typed as `NSObject`,
//!     read via `-[NSArray count]`/`-[NSArray objectAtIndex:]`) and testing
//!     each one with `-[NSObject isKindOfClass:]` against the
//!     `CAMetalLayer` class token, looked up purely by name at runtime
//!     (`objc2::runtime::AnyClass::get(c"CAMetalLayer")` -- again no
//!     `objc2-quartz-core` needed, since a class can be found by its
//!     Objective-C name with no Rust-side type for it at all).
//!     [`apply_reference_colorspace`] does exactly this, checking the root
//!     layer first (in case a future `wgpu`/`winit` upgrade ever changes
//!     which case applies) before falling back to the sublayer search.
//!  4. Once found, `-[CAMetalLayer setColorspace:]` (also raw `msg_send!`,
//!     for the same reason) sets the one property this task asks for:
//!     `colorspace`, a `CGColorSpaceRef`. That pointer comes from
//!     `apple_cf::cg::CGColorSpace` (already a direct dependency; `cg` is
//!     one of its `default` features) rather than `core-graphics` 0.25.0's
//!     own `CGColorSpace` wrapper (also already a direct dependency): the
//!     latter's raw pointer is reachable only through
//!     `foreign_types::ForeignType::as_ptr`, and `foreign-types` is --
//!     precisely the class of problem this whole section is about -- only
//!     a transitive dependency here, never a direct one.
//!     `apple_cf::cg::CGColorSpace` has no such wall: `::srgb()`/
//!     `::display_p3()` construct it and `::as_ptr(&self) -> *mut c_void`
//!     is a plain, already-public inherent method.
//!
//! What this does **not** unblock: the presentation surface's pixel
//! *format*. This pass re-confirmed the earlier finding with sharper
//! citations than before: `wgpu-hal-29.0.4/src/metal/adapter.rs`'s
//! `surface_capabilities()` only conditionally pushes
//! `wgt::TextureFormat::Rgb10a2Unorm` onto its supported-format list at all
//! (gated on `self.shared.private_texture_format_caps.format_rgb10a2_unorm_all`),
//! and even then only *after* `Bgra8Unorm`/`Bgra8UnormSrgb`/`Rgba16Float`,
//! which are pushed unconditionally and first; `egui-wgpu-0.35.0/src/lib.rs`'s
//! `preferred_framebuffer_format` scans that list in order for the first
//! `Rgba8Unorm`/`Bgra8Unorm` match, so `Bgra8Unorm` wins deterministically
//! whether or not the Metal device even supports a 10-bit swapchain (see
//! the regression test for this exact claim in the test module below).
//! `wgpu-hal-29.0.4/src/metal/surface.rs`'s `configure()` additionally calls
//! `render_layer.setPixelFormat(caps.map_format(config.format))` on every
//! surface configure (including every resize), so even reaching in and
//! forcing the live layer's `pixelFormat` to `MTLPixelFormatRGB10A2Unorm`
//! from this module -- mechanically possible with the exact same
//! `msg_send!` technique above -- would both get silently stomped back to
//! `Bgra8Unorm` on the very next resize *and*, in the meantime, desync the
//! drawable from the `Bgra8Unorm`-typed pipelines `egui_wgpu::Renderer`
//! already built against it, which is why [`apply_reference_colorspace`]
//! deliberately never sends `setPixelFormat:`. `colorspace` has no such
//! owner anywhere in `wgpu-hal-29.0.4/src/metal/`: it is never read, set,
//! or referenced by that module at all (checked directly, not inferred), so
//! it is the one property of this exact `CAMetalLayer` genuinely free for
//! application code to own.
//!
//! The zero-copy `CVMetalTextureCache` upload path in the section above is
//! unaffected by any of this -- it is a different blocker
//! (`objc2-metal`/`MTLTexture`, for uploading *into* a texture, not
//! presenting one) that this pass did not attempt to unblock, and remains
//! exactly as documented there.
//!
//! ## EDR/HDR: implemented, but on the other surface
//!
//! [`ReferenceColorSpace`] intentionally keeps only SDR variants
//! (`Srgb`/`DisplayP3`), and [`apply_reference_colorspace`] only ever sends
//! `setColorspace:`. That is now a deliberate *capability* boundary rather
//! than an unfinished seam: this module drives `egui`/`wgpu`'s own
//! `Bgra8Unorm` swapchain (see "swapchain itself stays 8-bit" above), and
//! tagging an eight-bit SDR drawable as PQ would have macOS tone-map the
//! whole window -- UI chrome included -- as if its 0-255 codes were
//! absolute luminance.
//!
//! HDR is therefore implemented on the surface that can carry it: the
//! dedicated `RGB10A2Unorm` `CAMetalLayer` in
//! [`super::video_metal_layer`], which sends
//! `setWantsExtendedDynamicRangeContent:` and attaches `CAEDRMetadata`.
//! [`PresentationColorSpace`] is the vocabulary for that decision and
//! [`presentation_colorspace_for`] makes it, keyed on the negotiated
//! transfer function. The two enums stay distinct so the eight-bit path
//! cannot be handed an HDR variant even by accident.
//!
//! # Compile status
//!
//! **None of this has been compiled, type-checked, or run.** This is
//! macOS-only code edited from Windows; the repository's own constraints
//! for this change permit only `rustfmt --edition 2024` (a parse check) as
//! verification. Every `wgpu`/`egui`/`egui_wgpu`/`objc2`/AppKit API used
//! here was instead read directly out of the vendored crate sources at the
//! exact pinned versions in `Cargo.lock` (`wgpu`/`wgpu-hal`/`wgpu-types`
//! 29.0.4, `egui`/`egui-wgpu`/`epaint` 0.35.0, `eframe` 0.35.0, `type-map`
//! 0.5.1, `apple-cf` 0.9.3, `objc2` 0.6.4, `objc2-app-kit` 0.3.2,
//! `objc2-foundation` 0.3.2, `core-graphics` 0.25.0; plus `raw-window-metal`
//! 1.1.0 and `winit` 0.30.13, read-only citations for the "Unblocked
//! 2026-08-14" section above, never new dependencies of this crate) rather
//! than assumed, and the pure data/uniform-construction/colour-space-choice/
//! fallback-retry logic has unit tests below, but nothing here has been
//! exercised end to end -- least of all [`apply_reference_colorspace`]
//! itself, which needs a live `NSApplication`/window and so cannot be unit
//! tested at all (matching this crate's existing AppKit-reaching functions,
//! e.g. `crate::ui::multi_window_runtime::window_display_id`, which also
//! have no unit tests for the same reason); only the pure decision logic
//! around it ([`reference_colorspace_for`], [`ColorspaceApplication`]) is
//! tested.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use eframe::egui_wgpu;
use eframe::egui_wgpu::wgpu;

use crate::pipeline::video_decoder::DecodedVideoFrame;

// ============================================================================
// Negotiated colour contract
// ============================================================================

/// The four negotiated axes that determine how a decoded plane must be
/// converted to RGB: chroma layout, coded sample range, coded bit depth, and
/// matrix coefficients. Mirrors `arcen_media::VideoConfiguration`'s own
/// axes (deliberately: this is meant to be built directly from whatever the
/// client already resolved for the wire request -- see
/// `effective_color_fidelity_variant` in `app.rs` -- rather than inventing a
/// parallel vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoColorContract {
    pub chroma: arcen_media::ChromaSubsampling,
    pub range: arcen_media::ColorRange,
    pub depth: arcen_media::BitDepth,
    pub matrix: arcen_media::ColorMatrix,
    /// Colour primaries -- unused by [`VideoUniform::from_contract`] (the
    /// YCbCr/GBR -> RGB math only needs `matrix`), but is what
    /// [`reference_colorspace_for`] keys the presentation `CAMetalLayer`'s
    /// working colour space on (see the module doc's "Unblocked 2026-08-14"
    /// section).
    pub primaries: arcen_media::ColorPrimaries,
    /// Transfer characteristics -- like `primaries`, unused by the
    /// YCbCr -> RGB math, and like it a presentation-surface input rather
    /// than a conversion one. This is the *only* axis that separates HDR
    /// from SDR: `Pq` means the coded values are absolute-luminance PQ
    /// (HDR10), `Bt709` means they are an SDR curve. Depth does not carry
    /// this information -- 4:4:4 10-bit BT.709 ("Grading Reference") is a
    /// real, useful, and entirely SDR configuration -- so anything keying
    /// HDR behaviour off `depth` would light up EDR for a grading session
    /// and tone-map it wrongly. See [`presentation_colorspace_for`].
    pub transfer: arcen_media::TransferCharacteristics,
}

impl Default for VideoColorContract {
    /// The legacy contract: BT.709, limited range, eight-bit, 4:2:0 --
    /// `arcen_media::VideoConfiguration::legacy_h264()`'s own axes.
    fn default() -> Self {
        Self {
            chroma: arcen_media::ChromaSubsampling::Yuv420,
            range: arcen_media::ColorRange::Limited,
            depth: arcen_media::BitDepth::Eight,
            matrix: arcen_media::ColorMatrix::Bt709,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        }
    }
}

impl From<arcen_media::VideoConfiguration> for VideoColorContract {
    fn from(configuration: arcen_media::VideoConfiguration) -> Self {
        Self {
            chroma: configuration.chroma,
            range: configuration.range,
            depth: configuration.bit_depth,
            matrix: configuration.matrix,
            primaries: configuration.primaries,
            transfer: configuration.transfer,
        }
    }
}

/// Luma coefficients `(Kr, Kb)` for a matrix (`Kg = 1 - Kr - Kb`).
///
/// Mirrors `arcen_media::video::convert::luma_weights`, which is private to
/// that crate (not `pub`), so it cannot be called from here directly. These
/// are the plain ITU-R BT.709/601/2020 constants, not anything
/// Arcen-specific, so duplicating them here (with this doc as the
/// cross-reference to the single real source of truth) is low-risk; if that
/// crate ever grows a public accessor this should call it instead.
const fn luma_weights(matrix: arcen_media::ColorMatrix) -> (f32, f32) {
    match matrix {
        arcen_media::ColorMatrix::Identity | arcen_media::ColorMatrix::Bt709 => (0.2126, 0.0722),
        arcen_media::ColorMatrix::Bt601 => (0.299, 0.114),
        arcen_media::ColorMatrix::Bt2020Ncl => (0.2627, 0.0593),
    }
}

// ============================================================================
// Shader uniform
// ============================================================================

/// Selects which branch `fs_main` (in `video_render.wgsl`) takes. Keep the
/// discriminants in sync with the `MODE_*` constants there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum ShaderMode {
    /// BT.709/601/2020 NCL YCbCr -> RGB.
    Matrix = 0,
    /// ITU-T H.273 identity/GBR: plane0 -> G, chroma-plane Cb-slot -> B,
    /// chroma-plane Cr-slot -> R, no matrix.
    IdentityGbr = 1,
    /// Already-decoded RGB (today's only reachable source); no conversion.
    PackedRgba = 2,
}

/// The `VideoUniform` uniform buffer's contents, field-for-field matching
/// `struct VideoUniform` in `video_render.wgsl`. See [`Self::to_bytes`] for
/// the byte layout this must produce.
///
/// `pub(crate)` (not `pub`, and not private): reused directly by
/// `video_metal_layer::MetalVideoUniform` (`w4-dedicated-metal-layer`'s
/// dedicated `CAMetalLayer` path) so the two shaders' shared
/// offset/span/matrix maths can never silently diverge -- see that
/// struct's own doc for why calling [`Self::from_contract`] directly, not
/// re-deriving the formula, is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VideoUniform {
    luma_offset: f32,
    luma_span_inv: f32,
    chroma_center: f32,
    chroma_span_inv: f32,
    kr: f32,
    kb: f32,
    kg_inv: f32,
    mode: u32,
    luma_width: f32,
    luma_height: f32,
    chroma_width: f32,
    chroma_height: f32,
}

impl VideoUniform {
    /// The uniform for [`RawVideoPayload::PackedRgba8`]: already-decoded
    /// RGB, no conversion at all (`ShaderMode::PackedRgba`). The
    /// range/matrix fields are unused by that branch and set to identity-
    /// safe values purely so this struct never carries a NaN/Inf-producing
    /// combination regardless of mode.
    fn passthrough(width: u32, height: u32) -> Self {
        Self {
            luma_offset: 0.0,
            luma_span_inv: 1.0,
            chroma_center: 0.0,
            chroma_span_inv: 1.0,
            kr: 0.0,
            kb: 0.0,
            kg_inv: 1.0,
            mode: ShaderMode::PackedRgba as u32,
            luma_width: width as f32,
            luma_height: height as f32,
            chroma_width: width as f32,
            chroma_height: height as f32,
        }
    }

    /// Derives the uniform for a negotiated [`VideoColorContract`] plus the
    /// luma/chroma plane pixel dimensions, exactly mirroring
    /// `arcen_media::video::convert::ColorTransform::new`'s own
    /// scale/offset/centre derivation (see the field docs on
    /// `struct VideoUniform` in `video_render.wgsl` for the formulas this
    /// feeds). `pub(crate)`: see `struct VideoUniform`'s own doc for why
    /// `video_metal_layer::MetalVideoUniform` calls this directly.
    pub(crate) fn from_contract(
        contract: VideoColorContract,
        luma_size: (u32, u32),
        chroma_size: (u32, u32),
    ) -> Self {
        let depth = contract.depth;
        let (luma_lo, luma_hi) = contract.range.luma_bounds(depth);
        let (chroma_lo, chroma_hi) = contract.range.chroma_bounds(depth);
        let luma_offset = f32::from(luma_lo);
        let luma_span = f32::from(luma_hi - luma_lo);
        let chroma_span = f32::from(chroma_hi - chroma_lo);
        // Full-range chroma centring uses the ITU convention
        // `1 << (depth.bits() - 1)` (e.g. 512 at ten bits), *not* the
        // arithmetic mean of `chroma_bounds()` (`(0 + 1023) / 2 = 511.5`,
        // which integer-truncates to 511) -- see `ColorTransform::new`'s own
        // `Full => (..., 1 << (depth.bits() - 1))` arm. Limited range's
        // bounds are always symmetric around it exactly (16 + 240 = 256),
        // so the mean is exact there.
        let chroma_center = match contract.range {
            arcen_media::ColorRange::Limited => f32::from(chroma_lo + chroma_hi) / 2.0,
            arcen_media::ColorRange::Full => (1u32 << (u32::from(depth.bits()) - 1)) as f32,
        };
        let (kr, kb) = luma_weights(contract.matrix);
        let kg = 1.0 - kr - kb;
        let mode = if contract.matrix.is_identity() {
            ShaderMode::IdentityGbr
        } else {
            ShaderMode::Matrix
        };
        Self {
            luma_offset,
            luma_span_inv: 1.0 / luma_span,
            chroma_center,
            chroma_span_inv: 1.0 / chroma_span,
            kr,
            kb,
            kg_inv: 1.0 / kg,
            mode: mode as u32,
            luma_width: luma_size.0 as f32,
            luma_height: luma_size.1 as f32,
            chroma_width: chroma_size.0 as f32,
            chroma_height: chroma_size.1 as f32,
        }
    }

    /// Byte layout matching `struct VideoUniform` in `video_render.wgsl`
    /// field-for-field: twelve plain 4-byte `f32`/`u32` scalars in
    /// declaration order. WGSL's `uniform` address space only forces
    /// 16-byte alignment on vector/struct/array *members*, none of which
    /// appear here, so this needs no padding between fields -- just a
    /// 48-byte (already a multiple of 16) total, built by hand rather than
    /// via a `#[repr(C)]`/`Pod` derive so this file needs no `bytemuck`
    /// dependency (not a direct dependency of this crate). `pub(crate)`:
    /// `video_metal_layer::MetalVideoUniform::to_bytes` appends its own
    /// Metal-only field after calling this directly.
    pub(crate) fn to_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(48);
        bytes.extend_from_slice(&self.luma_offset.to_le_bytes());
        bytes.extend_from_slice(&self.luma_span_inv.to_le_bytes());
        bytes.extend_from_slice(&self.chroma_center.to_le_bytes());
        bytes.extend_from_slice(&self.chroma_span_inv.to_le_bytes());
        bytes.extend_from_slice(&self.kr.to_le_bytes());
        bytes.extend_from_slice(&self.kb.to_le_bytes());
        bytes.extend_from_slice(&self.kg_inv.to_le_bytes());
        bytes.extend_from_slice(&self.mode.to_le_bytes());
        bytes.extend_from_slice(&self.luma_width.to_le_bytes());
        bytes.extend_from_slice(&self.luma_height.to_le_bytes());
        bytes.extend_from_slice(&self.chroma_width.to_le_bytes());
        bytes.extend_from_slice(&self.chroma_height.to_le_bytes());
        debug_assert_eq!(bytes.len(), 48);
        bytes
    }
}

// ============================================================================
// Raw payload shim (the interface `video_decoder.rs` needs to grow towards)
// ============================================================================

/// One CPU-side decoded plane at up to sixteen bits per component,
/// MSB-aligned exactly like
/// `arcen_media::video::convert::ColorTransform::pack_p16`/`unpack_p16`
/// (`code << (16 - depth.bits())`) -- CoreVideo's own P010/P210/P410
/// convention, so a future `video_decoder.rs` change can hand these bytes
/// over directly from a locked `CVPixelBuffer` plane with no repacking.
/// `components` is 1 for a luma plane, 2 for an interleaved chroma plane
/// (Cb then Cr; see the module doc on `plane1` in `video_render.wgsl`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneBuffer {
    pub width: usize,
    pub height: usize,
    pub components: u8,
    pub texels: Arc<[u16]>,
}

/// Rejected [`PlaneBuffer::new`] construction.
// Seam types/methods below (`PlaneBufferError`, `PlaneBuffer::new`,
// `RawVideoPayload::Planar16`, `RemoteVideoFrame::from_planar16`,
// `RemoteVideoFrame::size`) have no production caller yet -- see the module
// doc's "What is real today vs. what is a seam" section -- but are
// exercised directly by the unit tests below. `#[allow(dead_code)]` keeps
// that honest gap from generating build noise without hiding it (it is
// documented, not silent).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlaneBufferError {
    #[error("plane has a zero dimension: {width}x{height}")]
    ZeroDimension { width: usize, height: usize },
    #[error("plane {width}x{height}x{components} needs {expected} texels, got {actual}")]
    LengthMismatch {
        width: usize,
        height: usize,
        components: u8,
        expected: usize,
        actual: usize,
    },
}

impl PlaneBuffer {
    /// # Errors
    ///
    /// Returns [`PlaneBufferError`] if either dimension is zero, or if
    /// `texels.len() != width * height * components` -- exactly the
    /// invariant `video_decoder.rs`'s own `validate_biplanar_layout` checks
    /// for a locked `CVPixelBuffer` plane.
    #[allow(dead_code)] // seam -- see the module doc; exercised by tests below.
    pub fn new(
        width: usize,
        height: usize,
        components: u8,
        texels: Vec<u16>,
    ) -> Result<Self, PlaneBufferError> {
        if width == 0 || height == 0 {
            return Err(PlaneBufferError::ZeroDimension { width, height });
        }
        let expected = width
            .saturating_mul(height)
            .saturating_mul(usize::from(components));
        if texels.len() != expected {
            return Err(PlaneBufferError::LengthMismatch {
                width,
                height,
                components,
                expected,
                actual: texels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            components,
            texels: Arc::from(texels),
        })
    }
}

/// GPU-upload-ready description of one plane: raw bytes plus the `wgpu`
/// texture format/dimensions to create and upload it with. Component
/// layout is implied by `format` (e.g. `Rg16Uint` is a two-component
/// interleaved chroma plane).
struct PlaneUpload {
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    bytes: Vec<u8>,
    bytes_per_texel: u32,
}

impl PlaneUpload {
    /// A 1x1 placeholder for the bind group slot a given mode does not use
    /// (`plane1` under `ShaderMode::PackedRgba`): the bind group layout is
    /// shared across every mode, so every mode must bind *something* there.
    fn dummy_rg16() -> Self {
        Self {
            width: 1,
            height: 1,
            format: wgpu::TextureFormat::Rg16Uint,
            bytes: vec![0u8; 4],
            bytes_per_texel: 4,
        }
    }
}

fn u16_plane_to_le_bytes(texels: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(texels.len() * 2);
    for texel in texels {
        bytes.extend_from_slice(&texel.to_le_bytes());
    }
    bytes
}

/// The decoded video payload this module renders, prior to any RGB
/// conversion. See the module doc's "What is real today vs. what is a seam"
/// section: only [`Self::PackedRgba8`] is reachable from any production code
/// path today.
#[derive(Debug, Clone)]
pub enum RawVideoPayload {
    /// Already-converted interleaved RGBA8, exactly what
    /// `DecodedVideoFrame::rgba` carries today.
    PackedRgba8 {
        width: usize,
        height: usize,
        rgba: Arc<[u8]>,
    },
    /// Planar YCbCr (or GBR, for `ColorMatrix::Identity`) at up to sixteen
    /// bits/component, plus the contract needed to convert it. Not
    /// constructed by any production code path yet -- see the module doc.
    #[allow(dead_code)] // seam -- see the module doc; exercised by tests below.
    Planar16 {
        luma: PlaneBuffer,
        chroma: PlaneBuffer,
        contract: VideoColorContract,
    },
}

impl RawVideoPayload {
    /// The source's native pixel dimensions (the luma/RGB plane's own
    /// size), used both for the eventual GPU texture size and for
    /// `ArcenApp::remote_frame_size`-style aspect-ratio bookkeeping.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        match self {
            Self::PackedRgba8 { width, height, .. } => (*width, *height),
            Self::Planar16 { luma, .. } => (luma.width, luma.height),
        }
    }

    /// The negotiated colour primaries, when known. `None` for
    /// [`Self::PackedRgba8`] -- today's only reachable payload; see the
    /// module doc's "What is real today vs. what is a seam" section -- which
    /// carries no [`VideoColorContract`] at all. Feeds
    /// [`reference_colorspace_for`], the presentation `CAMetalLayer`
    /// colour-space choice (see the module doc's "Unblocked 2026-08-14"
    /// section).
    fn primaries(&self) -> Option<arcen_media::ColorPrimaries> {
        match self {
            Self::PackedRgba8 { .. } => None,
            Self::Planar16 { contract, .. } => Some(contract.primaries),
        }
    }

    fn uniform(&self) -> VideoUniform {
        match self {
            Self::PackedRgba8 { width, height, .. } => {
                VideoUniform::passthrough(*width as u32, *height as u32)
            }
            Self::Planar16 {
                luma,
                chroma,
                contract,
            } => VideoUniform::from_contract(
                *contract,
                (luma.width as u32, luma.height as u32),
                (chroma.width as u32, chroma.height as u32),
            ),
        }
    }

    fn plane_uploads(&self) -> (PlaneUpload, PlaneUpload) {
        match self {
            Self::PackedRgba8 {
                width,
                height,
                rgba,
            } => (
                PlaneUpload {
                    width: *width as u32,
                    height: *height as u32,
                    format: wgpu::TextureFormat::Rgba8Uint,
                    bytes: rgba.to_vec(),
                    bytes_per_texel: 4,
                },
                PlaneUpload::dummy_rg16(),
            ),
            Self::Planar16 { luma, chroma, .. } => (
                PlaneUpload {
                    width: luma.width as u32,
                    height: luma.height as u32,
                    format: wgpu::TextureFormat::R16Uint,
                    bytes: u16_plane_to_le_bytes(&luma.texels),
                    bytes_per_texel: 2,
                },
                PlaneUpload {
                    width: chroma.width as u32,
                    height: chroma.height as u32,
                    format: wgpu::TextureFormat::Rg16Uint,
                    bytes: u16_plane_to_le_bytes(&chroma.texels),
                    bytes_per_texel: 4,
                },
            ),
        }
    }
}

// ============================================================================
// `ArcenApp`-facing handle
// ============================================================================

static FRAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One decoded remote-video frame, ready to paint via a `wgpu` callback.
///
/// Replaces `egui::TextureHandle` as the type stored in
/// `ArcenApp::remote_texture`: constructing one does not touch the GPU at
/// all (no `wgpu::Device` needed), matching `egui::TextureHandle`'s own
/// "just a handle" cost so call sites that only ever checked
/// `.is_some()`/`.is_none()` need no changes. The actual `wgpu` resources
/// are created/updated lazily, the first time [`Self::paint`]'s callback is
/// actually processed by `egui_wgpu::Renderer`.
#[derive(Clone)]
pub struct RemoteVideoFrame {
    payload: Arc<RawVideoPayload>,
    /// Monotonic identity for this frame's payload, used by
    /// [`VideoRendererResources::update`] to skip re-uploading plane
    /// textures on a repaint that carries no new decoded frame (mirroring
    /// the old code's own "only touch the texture when `update_remote_texture`
    /// is actually called" cadence). A `u64` counter rather than comparing
    /// `Arc` pointers: the previous frame's `Arc` can be dropped (and its
    /// allocation reused by a later one) the instant `ArcenApp` replaces
    /// `remote_texture`, which would make pointer comparison an unsound
    /// false-negative hazard.
    sequence: u64,
}

impl RemoteVideoFrame {
    /// Builds the frame from today's real decoder output. Always selects
    /// [`RawVideoPayload::PackedRgba8`] -- see the module doc for why
    /// nothing richer is constructible yet.
    #[must_use]
    pub fn from_decoded(frame: DecodedVideoFrame) -> Self {
        Self::from_payload(RawVideoPayload::PackedRgba8 {
            width: frame.width,
            height: frame.height,
            rgba: Arc::from(frame.rgba),
        })
    }

    /// Builds the frame from an already-negotiated planar payload -- the
    /// seam a future `video_decoder.rs` change wires into. Not called from
    /// any production code path today.
    #[allow(dead_code)] // seam -- see the module doc; exercised by tests below.
    #[must_use]
    pub fn from_planar16(
        luma: PlaneBuffer,
        chroma: PlaneBuffer,
        contract: VideoColorContract,
    ) -> Self {
        Self::from_payload(RawVideoPayload::Planar16 {
            luma,
            chroma,
            contract,
        })
    }

    fn from_payload(payload: RawVideoPayload) -> Self {
        Self {
            payload: Arc::new(payload),
            sequence: FRAME_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The source's native pixel dimensions.
    #[allow(dead_code)]
    // not read by production code (`ArcenApp` tracks size
    // separately via `remote_frame_size`, computed straight from
    // `DecodedVideoFrame`); kept as public API and exercised by tests below.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        self.payload.size()
    }

    /// Paints this frame into `rect` via a `wgpu` paint callback, replacing
    /// `ui.painter().image(texture.id(), rect, uv, tint)` at the old call
    /// sites. Conversion (matrix/identity) and the eventual GPU upload
    /// happen later, inside `egui_wgpu::Renderer`'s own
    /// `CallbackTrait::prepare`/`paint` invocations -- this call itself
    /// touches no `wgpu` resource.
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            VideoPaintCallback {
                payload: Arc::clone(&self.payload),
                sequence: self.sequence,
            },
        ));
    }
}

// ============================================================================
// `egui_wgpu::CallbackTrait` implementation
// ============================================================================

struct VideoPaintCallback {
    payload: Arc<RawVideoPayload>,
    sequence: u64,
}

impl egui_wgpu::CallbackTrait for VideoPaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let resources = callback_resources
            .entry::<VideoRendererResources>()
            .or_insert_with(|| VideoRendererResources::new(device));
        resources.update(device, queue, egui_encoder, &self.payload, self.sequence);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(resources) = callback_resources.get::<VideoRendererResources>() {
            resources.draw(render_pass);
        }
    }
}

// ============================================================================
// Persistent GPU resources (lives in `egui_wgpu::CallbackResources`)
// ============================================================================

/// One `wgpu` texture bound as a plane input, recreated only when its
/// format/dimensions actually change.
struct PlaneSlot {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
}

impl PlaneSlot {
    fn create(
        device: &wgpu::Device,
        label: &str,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            format,
            width,
            height,
        }
    }

    /// (Re)creates the texture if `format`/`width`/`height` differ from what
    /// is currently bound. Returns whether it was recreated (the caller
    /// must then rebuild any bind group referencing [`Self::view`]).
    fn ensure(
        &mut self,
        device: &wgpu::Device,
        label: &str,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> bool {
        if self.format == format && self.width == width && self.height == height {
            return false;
        }
        *self = Self::create(device, label, format, width, height);
        true
    }

    fn write(&self, queue: &wgpu::Queue, bytes: &[u8], bytes_per_texel: u32) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * bytes_per_texel),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// The `Rgb10a2Unorm` resolve target `fs_main` converts into -- the actual
/// SDR 10-bit reference-viewing surface (see the module doc's "swapchain
/// itself stays 8-bit" section for what this is and is not).
struct ResolvedTarget {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl ResolvedTarget {
    fn create(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("arcen-video-resolved-10bit"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // SDR 10-bit reference-viewing target: verified (see the module
            // doc) to need no extra `wgpu::Features`, and to be a native
            // `RENDER_ATTACHMENT` + filterable-`Float`-sample-type format
            // that `wgpu-hal`'s Metal backend maps straight to
            // `MTLPixelFormatRGB10A2Unorm`.
            format: wgpu::TextureFormat::Rgb10a2Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            view,
            width,
            height,
        }
    }

    fn ensure(&mut self, device: &wgpu::Device, width: u32, height: u32) -> bool {
        if self.width == width && self.height == height {
            return false;
        }
        *self = Self::create(device, width, height);
        true
    }
}

struct VideoRendererResources {
    convert_pipeline: wgpu::RenderPipeline,
    convert_bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    plane0: PlaneSlot,
    plane1: PlaneSlot,
    convert_bind_group: wgpu::BindGroup,
    resolved: ResolvedTarget,
    composite_pipeline: wgpu::RenderPipeline,
    composite_bind_group_layout: wgpu::BindGroupLayout,
    composite_sampler: wgpu::Sampler,
    composite_bind_group: wgpu::BindGroup,
    last_sequence: Option<u64>,
    /// Tracks/de-duplicates the presentation `CAMetalLayer` colour-space
    /// application; see [`ensure_reference_colorspace`][Self::ensure_reference_colorspace].
    colorspace_application: ColorspaceApplication,
}

/// The presentation surface's actual pixel format on every Mac this app
/// runs on today. See the module doc's "swapchain itself stays 8-bit"
/// section for the exact `egui-wgpu`/`wgpu-hal` source this is derived
/// from: it is not a guess, but it is also not independently reachable or
/// overridable from here, hence the `const` (nothing to plumb it through).
const SWAPCHAIN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

impl VideoRendererResources {
    fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("arcen-video-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video_render.wgsl").into()),
        });

        let convert_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("arcen-video-convert-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Uint,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Uint,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let convert_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("arcen-video-convert-pipeline-layout"),
                bind_group_layouts: &[Some(&convert_bind_group_layout)],
                immediate_size: 0,
            });
        let convert_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("arcen-video-convert-pipeline"),
            layout: Some(&convert_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgb10a2Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("arcen-video-uniform"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let plane0 = PlaneSlot::create(
            device,
            "arcen-video-plane0",
            wgpu::TextureFormat::Rgba8Uint,
            1,
            1,
        );
        let plane1 = PlaneSlot::create(
            device,
            "arcen-video-plane1",
            wgpu::TextureFormat::Rg16Uint,
            1,
            1,
        );
        let convert_bind_group = Self::build_convert_bind_group(
            device,
            &convert_bind_group_layout,
            &plane0,
            &plane1,
            &uniform_buffer,
        );

        let resolved = ResolvedTarget::create(device, 1, 1);
        let composite_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("arcen-video-composite-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let composite_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("arcen-video-composite-pipeline-layout"),
                bind_group_layouts: &[Some(&composite_bind_group_layout)],
                immediate_size: 0,
            });
        let composite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("arcen-video-composite-pipeline"),
            layout: Some(&composite_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_composite"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    // See `SWAPCHAIN_FORMAT`'s own doc.
                    format: SWAPCHAIN_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let composite_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("arcen-video-composite-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let composite_bind_group = Self::build_composite_bind_group(
            device,
            &composite_bind_group_layout,
            &resolved,
            &composite_sampler,
        );

        Self {
            convert_pipeline,
            convert_bind_group_layout,
            uniform_buffer,
            plane0,
            plane1,
            convert_bind_group,
            resolved,
            composite_pipeline,
            composite_bind_group_layout,
            composite_sampler,
            composite_bind_group,
            last_sequence: None,
            colorspace_application: ColorspaceApplication::default(),
        }
    }

    fn build_convert_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        plane0: &PlaneSlot,
        plane1: &PlaneSlot,
        uniform_buffer: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("arcen-video-convert-bind-group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&plane0.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&plane1.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        })
    }

    fn build_composite_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        resolved: &ResolvedTarget,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("arcen-video-composite-bind-group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&resolved.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    /// Uploads/converts `payload` if it is a genuinely new frame (`sequence`
    /// differs from the last call), then always refreshes the uniform
    /// buffer (cheap; keeps this correct even for a hypothetical future
    /// caller that mutates uniform-affecting state without a new frame).
    /// Also (re)applies the presentation `CAMetalLayer`'s colour space every
    /// call until that first succeeds -- see
    /// [`ensure_reference_colorspace`][Self::ensure_reference_colorspace].
    fn update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        payload: &RawVideoPayload,
        sequence: u64,
    ) {
        self.ensure_reference_colorspace(payload);
        queue.write_buffer(&self.uniform_buffer, 0, &payload.uniform().to_bytes());
        if self.last_sequence == Some(sequence) {
            return;
        }
        self.last_sequence = Some(sequence);

        let (plane0_upload, plane1_upload) = payload.plane_uploads();
        let mut convert_rebuilt = false;
        convert_rebuilt |= self.plane0.ensure(
            device,
            "arcen-video-plane0",
            plane0_upload.format,
            plane0_upload.width,
            plane0_upload.height,
        );
        convert_rebuilt |= self.plane1.ensure(
            device,
            "arcen-video-plane1",
            plane1_upload.format,
            plane1_upload.width,
            plane1_upload.height,
        );
        self.plane0
            .write(queue, &plane0_upload.bytes, plane0_upload.bytes_per_texel);
        self.plane1
            .write(queue, &plane1_upload.bytes, plane1_upload.bytes_per_texel);
        if convert_rebuilt {
            self.convert_bind_group = Self::build_convert_bind_group(
                device,
                &self.convert_bind_group_layout,
                &self.plane0,
                &self.plane1,
                &self.uniform_buffer,
            );
        }

        let (width, height) = payload.size();
        #[allow(clippy::cast_possible_truncation)]
        let resolved_rebuilt = self.resolved.ensure(device, width as u32, height as u32);
        if resolved_rebuilt {
            self.composite_bind_group = Self::build_composite_bind_group(
                device,
                &self.composite_bind_group_layout,
                &self.resolved,
                &self.composite_sampler,
            );
        }

        // Pass 1 ("convert"): plane0/plane1 -> the resolved 10-bit target.
        // Recorded directly onto `egui`'s own shared encoder -- its own doc
        // explicitly allows this ("can be used directly to register wgpu
        // commands for simple use cases").
        let mut convert_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("arcen-video-convert-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.resolved.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        convert_pass.set_pipeline(&self.convert_pipeline);
        convert_pass.set_bind_group(0, &self.convert_bind_group, &[]);
        convert_pass.draw(0..3, 0..1);
    }

    /// Pass 2 ("present"): bilinear-resolves the 10-bit target into
    /// whichever render pass the caller (`egui_wgpu::Renderer`) is
    /// currently recording -- see [`SWAPCHAIN_FORMAT`]'s own doc for what
    /// format that actually is.
    fn draw(&self, render_pass: &mut wgpu::RenderPass<'static>) {
        render_pass.set_pipeline(&self.composite_pipeline);
        render_pass.set_bind_group(0, &self.composite_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }

    /// Applies (once per distinct [`ReferenceColorSpace`] choice, retrying
    /// every call until that first succeeds, then never again unless the
    /// desired choice itself later changes) the presentation
    /// `CAMetalLayer`'s working colour space. This is the only
    /// AppKit-touching step anywhere in this module -- everything else here
    /// is pure `wgpu`. See [`apply_reference_colorspace`] for exactly what
    /// it reaches and why, and the module doc's "Unblocked 2026-08-14"
    /// section for the full picture, including what this deliberately does
    /// *not* do: no pixel-format change, no EDR/HDR.
    fn ensure_reference_colorspace(&mut self, payload: &RawVideoPayload) {
        let desired = reference_colorspace_for(payload.primaries());
        if !self.colorspace_application.needs_attempt(desired) {
            return;
        }
        let outcome = apply_reference_colorspace(desired);
        let Some(logged_outcome) = self.colorspace_application.record(desired, outcome) else {
            // Same outcome as last time this was logged -- fail loud once
            // per distinct reason, not every frame at 60-120Hz.
            return;
        };
        if logged_outcome == ColorspaceOutcome::Applied {
            tracing::info!(
                target: crate::logging::target::VIDEO,
                ?desired,
                "tagged the presentation CAMetalLayer's colour space for SDR reference viewing",
            );
        } else {
            tracing::warn!(
                target: crate::logging::target::VIDEO,
                ?desired,
                ?logged_outcome,
                "could not tag the presentation CAMetalLayer's colour space; the video \
                 surface is falling back to the system default colour space -- colours are \
                 NOT confirmed to match the intended SDR reference space until this succeeds",
            );
        }
    }
}

// ============================================================================
// Presentation `CAMetalLayer` colour space (w4-10bit-drawable)
// ============================================================================
//
// See the module doc's "Unblocked 2026-08-14" section for the full
// evidence trail. Summary: the swapchain's pixel *format* stays
// `Bgra8Unorm` (owned entirely by `eframe`/`wgpu-hal`, re-asserted on every
// resize -- not touched here), but its `colorspace` has no owner anywhere
// in `wgpu-hal`, so this section tags it with the working colour space that
// matches the negotiated content, entirely via raw `objc2::msg_send!` (no
// `objc2-quartz-core`/`objc2-metal`/`raw-window-handle` dependency needed).

/// Which CoreGraphics working colour space the presentation `CAMetalLayer`
/// should be tagged with for SDR reference viewing. Deliberately has no
/// EDR/HDR variant -- see the module doc's EDR/HDR seam note (`w4-edr`).
///
/// `pub(crate)`: also the colour-space choice
/// `video_metal_layer::DedicatedVideoLayer` uses for *its own*,
/// independent `CAMetalLayer` (`w4-dedicated-metal-layer`) -- reused
/// directly via [`reference_colorspace_for`], not reimplemented, so the two
/// independent presentation paths this crate now has can never disagree
/// about which working colour space a given `ColorPrimaries` maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReferenceColorSpace {
    /// The universal, narrow-gamut default. Correct for
    /// [`arcen_media::ColorPrimaries::Bt709`] content, which sRGB's
    /// primaries were defined to match exactly.
    Srgb,
    /// Apple's own bounded wide-gamut default. The closest working space
    /// [`apple_cf::cg::CGColorSpace`] exposes to
    /// [`arcen_media::ColorPrimaries::DisplayP3`] (exact) or
    /// [`arcen_media::ColorPrimaries::Bt2020`] (wider than P3; true Rec.
    /// 2020 is not wrapped there, and chasing it exactly is out of scope
    /// for SDR reference viewing -- see the EDR/HDR seam note).
    DisplayP3,
}

/// Chooses [`ReferenceColorSpace`] from the negotiated colour primaries,
/// when known. `primaries` is `None` for every payload reachable from
/// production code today ([`RawVideoPayload::PackedRgba8`] carries no
/// [`VideoColorContract`] at all -- see the module doc's "What is real
/// today" section), which conservatively resolves to
/// [`ReferenceColorSpace::Srgb`], matching [`VideoColorContract::default`]'s
/// own BT.709 legacy assumption (and
/// `arcen_media::VideoConfiguration::legacy_h264`'s). `Some(DisplayP3)`/
/// `Some(Bt2020)` -- reachable once a future `video_decoder.rs` change
/// wires up [`RawVideoPayload::Planar16`] -- resolve to
/// [`ReferenceColorSpace::DisplayP3`]; `Some(Bt709)` resolves to
/// [`ReferenceColorSpace::Srgb`]. `pub(crate)`: see `enum
/// ReferenceColorSpace`'s own doc for the second, independent caller.
pub(crate) fn reference_colorspace_for(
    primaries: Option<arcen_media::ColorPrimaries>,
) -> ReferenceColorSpace {
    match primaries {
        Some(arcen_media::ColorPrimaries::DisplayP3 | arcen_media::ColorPrimaries::Bt2020) => {
            ReferenceColorSpace::DisplayP3
        }
        _ => ReferenceColorSpace::Srgb,
    }
}

/// What a presentation surface should actually be tagged as, once the
/// negotiated transfer function is taken into account as well as the
/// primaries.
///
/// Deliberately a *separate* type from [`ReferenceColorSpace`] rather than
/// another variant on it, because the two presentation surfaces this crate
/// drives are not equally capable and must not be told the same thing:
///
/// - `egui`/`wgpu`'s own surface is an `Bgra8Unorm` swapchain (see the
///   module doc's "swapchain itself stays 8-bit" section for why that
///   cannot be changed from here). Tagging an eight-bit SDR drawable as PQ
///   would have macOS tone-map the entire UI -- window chrome included --
///   as if its 0-255 codes were absolute luminance. That surface therefore
///   keeps using [`reference_colorspace_for`] and stays SDR forever, by
///   construction rather than by remembering to.
/// - The dedicated `CAMetalLayer` (`super::video_metal_layer`) is a real
///   `RGB10A2Unorm` drawable carrying only video, which is exactly the
///   surface HDR10 wants. It is the only caller of this function.
///
/// Making the enums distinct means the eight-bit path cannot be handed an
/// HDR variant even by accident: it does not exist in its vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentationColorSpace {
    /// Standard dynamic range; the surface is tagged with the given
    /// SDR working space and EDR stays off.
    Sdr(ReferenceColorSpace),
    /// HDR10: BT.2020 primaries with the SMPTE ST 2084 (PQ) transfer,
    /// tagged `kCGColorSpaceITUR_2100_PQ`, with
    /// `wantsExtendedDynamicRangeContent` on and `CAEDRMetadata` attached.
    Hdr10Pq,
}

/// Chooses the presentation colour space from the two axes that decide it.
///
/// **`transfer` is what makes this HDR, not `depth` and not `primaries`.**
/// BT.2020 primaries with a BT.709 transfer is a wide-gamut *SDR* signal;
/// ten-bit BT.709 is `Grading Reference`, also SDR (its extra bits buy
/// banding headroom, not dynamic range). Only [`Pq`][pq] means the coded
/// values are absolute luminance, so only `Pq` may turn EDR on. Claiming
/// otherwise makes macOS tone-map an SDR stream against a 1000-nit curve
/// and the picture comes back visibly wrong -- dark and desaturated --
/// which is precisely the failure this split exists to prevent.
///
/// [pq]: arcen_media::TransferCharacteristics::Pq
pub(crate) fn presentation_colorspace_for(
    primaries: arcen_media::ColorPrimaries,
    transfer: arcen_media::TransferCharacteristics,
) -> PresentationColorSpace {
    match transfer {
        arcen_media::TransferCharacteristics::Pq => PresentationColorSpace::Hdr10Pq,
        _ => PresentationColorSpace::Sdr(reference_colorspace_for(Some(primaries))),
    }
}

/// The result of one attempt to reach and tag the presentation
/// `CAMetalLayer`'s colour space. Every non-[`Applied`][Self::Applied]
/// variant is a distinct, precise reason so a fallback is always
/// diagnosable rather than a single opaque "failed" (see the module doc's
/// "fail safe and loud" requirement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorspaceOutcome {
    /// `-[CAMetalLayer setColorspace:]` was sent successfully.
    Applied,
    /// Not called from the main thread; every AppKit call here requires it.
    NotMainThread,
    /// No open `NSWindow` currently has
    /// [`crate::ui::multi_window_runtime::ROOT_WINDOW_TITLE`] as its title
    /// (e.g. called before the root viewport has finished opening).
    NoRootWindow,
    /// The root window currently has no content view.
    NoContentView,
    /// `-[NSView layer]` returned `nil` (the view is not yet layer-backed;
    /// `wgpu` makes it so as part of creating its surface, so this should
    /// only be possible on a frame rendered before that surface exists).
    NoRootLayer,
    /// The `CAMetalLayer` Objective-C class is not currently registered
    /// with the runtime at all (never expected on a real macOS process with
    /// QuartzCore loaded, which every AppKit window requires).
    NoMetalLayerClass,
    /// Neither the content view's root layer nor any of its direct
    /// sublayers is a `CAMetalLayer` (e.g. `wgpu`'s surface has not been
    /// created yet on this frame).
    NoMetalSublayer,
    /// `CGColorSpaceCreateWithName` returned `nil` for a built-in system
    /// colour space. Never expected, but that constructor is fallible and
    /// silently keeping the layer's previous space would be worse than
    /// saying so.
    ColorSpaceUnavailable,
}

/// Tracks whether/what colour space has been successfully applied to the
/// presentation `CAMetalLayer`, and de-duplicates the "could not apply"
/// warning so a persistent failure (e.g. no window yet on the first frame
/// or two) logs once per distinct reason rather than every single frame at
/// 60-120Hz. Deliberately a standalone, plain-data type with no `wgpu`/
/// `objc2` in it (unlike [`apply_reference_colorspace`], which needs a live
/// `NSApplication`/window and so cannot run inside a unit test at all) so
/// this retry/log-once decision -- "the fallback decision" -- is exercised
/// directly by the tests below.
#[derive(Debug, Default)]
struct ColorspaceApplication {
    applied: Option<ReferenceColorSpace>,
    last_logged_outcome: Option<ColorspaceOutcome>,
}

impl ColorspaceApplication {
    /// Whether [`apply_reference_colorspace`] needs to (re)run this frame:
    /// only when `desired` has never been successfully applied yet.
    fn needs_attempt(&self, desired: ReferenceColorSpace) -> bool {
        self.applied != Some(desired)
    }

    /// Records one attempt's outcome. Returns `Some(outcome)` exactly when
    /// this is new information worth logging -- the very first attempt, a
    /// change from failure to success, a change from success to a *new*
    /// desired colour space, or a change to a different failure reason --
    /// and `None` on a repeat of the same outcome, so a caller logging
    /// whatever this returns never spams an identical line every frame.
    fn record(
        &mut self,
        desired: ReferenceColorSpace,
        outcome: ColorspaceOutcome,
    ) -> Option<ColorspaceOutcome> {
        if outcome == ColorspaceOutcome::Applied {
            self.applied = Some(desired);
        }
        if self.last_logged_outcome == Some(outcome) {
            return None;
        }
        self.last_logged_outcome = Some(outcome);
        Some(outcome)
    }
}

/// Finds the root viewport's `NSWindow` by title (mirroring
/// `crate::ui::multi_window_runtime::window_display_id`'s own
/// window-by-title loop exactly -- a plain `for` over `app.windows()`,
/// rather than `.iter()`/`.find()`, matches how that already-working lookup
/// is written).
///
/// `pub(crate)`: shared by [`apply_reference_colorspace`] and
/// `video_metal_layer::DedicatedVideoLayer::attach`
/// (`w4-dedicated-metal-layer`'s own, independent `CAMetalLayer`), so the
/// two `CAMetalLayer`-reaching paths this crate now has can never disagree
/// about which window is "the root viewport".
pub(crate) fn find_root_window(
    mtm: objc2_foundation::MainThreadMarker,
) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let app = NSApplication::sharedApplication(mtm);
    let root_title = NSString::from_str(crate::ui::multi_window_runtime::ROOT_WINDOW_TITLE);
    for window in app.windows() {
        if window.title().isEqualToString(&root_title) {
            return Some(window);
        }
    }
    None
}

/// Reaches the root viewport's content view's `CAMetalLayer`, and tags its
/// `colorspace`. See the module doc's "Unblocked 2026-08-14" section for
/// exactly why each step below is safe and how it was verified against the
/// vendored `objc2`/`objc2-app-kit`/`apple-cf`/`raw-window-metal` sources.
///
/// Does *not* touch `pixelFormat`: that property is `wgpu-hal`'s own to set
/// (re-asserted on every resize), and overwriting it out from under `wgpu`
/// would desync the drawable from the render pipelines `egui_wgpu::Renderer`
/// already built against `Bgra8Unorm` -- see the module doc.
///
/// # SAFETY
///
/// Every raw message send below has its own comment; the invariant common
/// to all of them is that the receiver is a live object obtained a moment
/// earlier either from a typed, safe `objc2_app_kit`/`objc2_foundation`
/// accessor, or from a previous successful raw send earlier in this same
/// function, so it is never dangling for the duration of the call. Every
/// selector/argument/return-type pairing (`-[NSView layer]`,
/// `-[CALayer sublayers]`, `-[NSArray count]`, `-[NSArray objectAtIndex:]`,
/// `-[NSObject isKindOfClass:]`, `-[CAMetalLayer setColorspace:]`) is a
/// real, stable AppKit/QuartzCore API taking/returning exactly `id`,
/// `NSUInteger`, a `Class`, or a toll-free `CGColorSpaceRef` -- matched here
/// by `NSObject`, `usize`, `&AnyClass`, and `*mut c_void` respectively, all
/// confirmed `Encode`/`EncodeReturn` in `objc2` 0.6.4's own `encode.rs`.
fn apply_reference_colorspace(colorspace: ReferenceColorSpace) -> ColorspaceOutcome {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, NSObject, NSObjectProtocol};
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(mtm) = MainThreadMarker::new() else {
        return ColorspaceOutcome::NotMainThread;
    };
    let Some(window) = find_root_window(mtm) else {
        return ColorspaceOutcome::NoRootWindow;
    };
    let Some(view) = window.contentView() else {
        return ColorspaceOutcome::NoContentView;
    };

    // SAFETY: `view` is the live `Retained<NSView>` just obtained above;
    // `-[NSView layer]` takes no arguments and returns an optional `id` (the
    // view's root `CALayer`, or nil if not yet layer-backed). `NSObject`
    // stands in for the not-yet-typed `CALayer` return, exactly like
    // `raw-window-metal` 1.1.0's own identical `msg_send![ns_view, layer]`
    // (see the module doc).
    let root_layer: Option<Retained<NSObject>> = unsafe { msg_send![&*view, layer] };
    let Some(root_layer) = root_layer else {
        return ColorspaceOutcome::NoRootLayer;
    };

    let Some(metal_class) = AnyClass::get(c"CAMetalLayer") else {
        return ColorspaceOutcome::NoMetalLayerClass;
    };
    let metal_layer = if root_layer.isKindOfClass(metal_class) {
        Some(root_layer)
    } else {
        // SAFETY: `root_layer` is the live `CALayer` just retained above;
        // `-[CALayer sublayers]` takes no arguments and returns an optional
        // `NSArray *` (`id`), read the same way as `layer` above.
        let sublayers: Option<Retained<NSObject>> = unsafe { msg_send![&*root_layer, sublayers] };
        // `w4-dedicated-metal-layer`'s own, independent `CAMetalLayer`
        // (`video_metal_layer.rs`) can now *also* be a sublayer here; it is
        // tagged with this name (`CALayer.name`) specifically so the loop
        // below can skip it -- see that constant's own doc. Without this
        // check, this search could find *that* layer first by sublayer
        // order and misapply the wgpu/egui surface's own colour space to
        // it instead (or vice versa), since both are plain `CAMetalLayer`s
        // and `isKindOfClass` alone cannot tell them apart.
        let dedicated_layer_name =
            NSString::from_str(crate::ui::video_metal_layer::DEDICATED_VIDEO_LAYER_NAME);
        sublayers.and_then(|sublayers| {
            // SAFETY: `sublayers` is the live `NSArray` just retained above;
            // `-[NSArray count]` takes no arguments and returns `NSUInteger`.
            let count: usize = unsafe { msg_send![&*sublayers, count] };
            (0..count).find_map(|index| {
                // SAFETY: `sublayers` is that same live `NSArray`, and
                // `index` is always `< count` from the range above, matching
                // `-[NSArray objectAtIndex:]`'s own bounds contract; it
                // takes one `NSUInteger` and returns a non-optional `id`.
                let sublayer: Retained<NSObject> =
                    unsafe { msg_send![&*sublayers, objectAtIndex: index] };
                if !sublayer.isKindOfClass(metal_class) {
                    return None;
                }
                // SAFETY: `sublayer` is that same live, just-retained
                // object, now known to be a `CALayer` (checked immediately
                // above); `-[CALayer name]` takes no arguments and returns
                // an optional `NSString *` (`id`).
                let name: Option<Retained<NSString>> = unsafe { msg_send![&*sublayer, name] };
                let is_dedicated_layer =
                    name.is_some_and(|name| name.isEqualToString(&dedicated_layer_name));
                (!is_dedicated_layer).then_some(sublayer)
            })
        })
    };
    let Some(metal_layer) = metal_layer else {
        return ColorspaceOutcome::NoMetalSublayer;
    };

    let color_space = match colorspace {
        ReferenceColorSpace::Srgb => objc2_core_graphics::CGColorSpace::with_name(Some(unsafe {
            objc2_core_graphics::kCGColorSpaceSRGB
        })),
        ReferenceColorSpace::DisplayP3 => {
            objc2_core_graphics::CGColorSpace::with_name(Some(unsafe {
                objc2_core_graphics::kCGColorSpaceDisplayP3
            }))
        }
    };
    let Some(color_space) = color_space else {
        return ColorspaceOutcome::ColorSpaceUnavailable;
    };
    // SAFETY: `metal_layer` is the live `CAMetalLayer` just found above;
    // `-[CAMetalLayer setColorspace:]` takes one `CGColorSpaceRef` and
    // returns nothing. `CALayer.colorspace` is a `retain` property (Apple's
    // own convention for every Core Foundation-backed Cocoa setter), so it
    // takes its own reference; `color_space` is safely dropped (releasing
    // this function's own reference) right after.
    //
    // The argument is a *typed* `&CGColorSpace`, not the untyped
    // `*mut c_void` this call passed until it was first exercised on
    // hardware. `objc2`'s `msg_send!` verifies argument type encodings
    // whenever `debug_assertions` are on, and a raw `c_void` pointer
    // encodes as `^v` where the selector declares `^{CGColorSpace=}`, so
    // every debug build panicked the moment a colour space was applied to
    // a live layer -- and every release build sailed past the same
    // mismatch silently, which is why it survived this long.
    let _: () = unsafe { msg_send![&*metal_layer, setColorspace: &*color_space] };
    ColorspaceOutcome::Applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_f32_near(actual: f32, expected: f32, context: &str) {
        let tolerance = expected.abs().max(1.0) * f32::EPSILON;
        assert!(
            (actual - expected).abs() <= tolerance,
            "{context}: expected {expected}, got {actual}"
        );
    }

    fn contract(
        chroma: arcen_media::ChromaSubsampling,
        range: arcen_media::ColorRange,
        depth: arcen_media::BitDepth,
        matrix: arcen_media::ColorMatrix,
    ) -> VideoColorContract {
        VideoColorContract {
            chroma,
            range,
            depth,
            matrix,
            // No existing caller of this helper exercises `primaries` (only
            // `VideoUniform::from_contract`, which never reads it) -- default
            // it to BT.709, matching `VideoColorContract::default`'s own
            // legacy assumption, rather than growing every one of this
            // helper's call sites for a field they don't test. Tests that
            // specifically exercise `primaries` (below) build their own
            // `VideoColorContract` literal instead of using this helper.
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        }
    }

    // ---- VideoUniform::from_contract: range bounds at each depth --------

    #[test]
    fn bt709_limited_eight_bit_matches_classic_16_235_240_bounds() {
        let c = contract(
            arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::ColorRange::Limited,
            arcen_media::BitDepth::Eight,
            arcen_media::ColorMatrix::Bt709,
        );
        let u = VideoUniform::from_contract(c, (1920, 1080), (960, 540));
        assert_eq!(u.luma_offset, 16.0);
        assert_f32_near(1.0 / u.luma_span_inv, 219.0, "235 - 16");
        assert_eq!(u.chroma_center, 128.0, "(16 + 240) / 2");
        assert_f32_near(1.0 / u.chroma_span_inv, 224.0, "240 - 16");
        assert_eq!(u.mode, ShaderMode::Matrix as u32);
    }

    #[test]
    fn limited_range_bounds_scale_linearly_with_depth() {
        let eight = contract(
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::ColorRange::Limited,
            arcen_media::BitDepth::Eight,
            arcen_media::ColorMatrix::Bt709,
        );
        let ten = contract(
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::ColorRange::Limited,
            arcen_media::BitDepth::Ten,
            arcen_media::ColorMatrix::Bt709,
        );
        let u8_bit = VideoUniform::from_contract(eight, (100, 100), (100, 100));
        let u10_bit = VideoUniform::from_contract(ten, (100, 100), (100, 100));
        assert_eq!(u10_bit.luma_offset, u8_bit.luma_offset * 4.0, "16<<2 = 64");
        assert_eq!(
            1.0 / u10_bit.luma_span_inv,
            (1.0 / u8_bit.luma_span_inv) * 4.0,
            "219<<2"
        );
        assert_eq!(u10_bit.chroma_center, u8_bit.chroma_center * 4.0);
        assert_eq!(
            1.0 / u10_bit.chroma_span_inv,
            (1.0 / u8_bit.chroma_span_inv) * 4.0
        );
    }

    #[test]
    fn full_range_chroma_centre_is_the_itu_midpoint_not_the_bounds_average() {
        let ten_bit_full = contract(
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::ColorRange::Full,
            arcen_media::BitDepth::Ten,
            arcen_media::ColorMatrix::Bt709,
        );
        let u = VideoUniform::from_contract(ten_bit_full, (100, 100), (100, 100));
        // The naive mean of `chroma_bounds()` (0, 1023) is 511.5 -- the ITU
        // convention this must use instead is `1 << (10 - 1) = 512`.
        assert_eq!(u.chroma_center, 512.0);
        assert_eq!(1.0 / u.chroma_span_inv, 1023.0, "max_code at ten bits");
        assert_eq!(u.luma_offset, 0.0);
        assert_eq!(1.0 / u.luma_span_inv, 1023.0);
    }

    #[test]
    fn twelve_bit_full_range_uses_the_4095_code_space() {
        let c = contract(
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::ColorRange::Full,
            arcen_media::BitDepth::Twelve,
            arcen_media::ColorMatrix::Bt2020Ncl,
        );
        let u = VideoUniform::from_contract(c, (100, 100), (100, 100));
        assert_f32_near(
            1.0 / u.luma_span_inv,
            4095.0,
            "full-range twelve-bit luma span",
        );
        assert_eq!(u.chroma_center, 2048.0, "1 << 11");
    }

    // ---- matrix coefficients ---------------------------------------------

    #[test]
    fn matrix_coefficients_match_the_itu_r_constants_per_matrix() {
        let bt709 = VideoUniform::from_contract(
            contract(
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::ColorRange::Full,
                arcen_media::BitDepth::Eight,
                arcen_media::ColorMatrix::Bt709,
            ),
            (10, 10),
            (10, 10),
        );
        assert!((bt709.kr - 0.2126).abs() < 1e-6);
        assert!((bt709.kb - 0.0722).abs() < 1e-6);

        let bt601 = VideoUniform::from_contract(
            contract(
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::ColorRange::Full,
                arcen_media::BitDepth::Eight,
                arcen_media::ColorMatrix::Bt601,
            ),
            (10, 10),
            (10, 10),
        );
        assert!((bt601.kr - 0.299).abs() < 1e-6);
        assert!((bt601.kb - 0.114).abs() < 1e-6);

        let bt2020 = VideoUniform::from_contract(
            contract(
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::ColorRange::Full,
                arcen_media::BitDepth::Eight,
                arcen_media::ColorMatrix::Bt2020Ncl,
            ),
            (10, 10),
            (10, 10),
        );
        assert!((bt2020.kr - 0.2627).abs() < 1e-6);
        assert!((bt2020.kb - 0.0593).abs() < 1e-6);

        // Kg = 1 - Kr - Kb for BT.709: 1 - 0.2126 - 0.0722 = 0.7152.
        assert!((1.0 / bt709.kg_inv - 0.7152).abs() < 1e-6);
    }

    // ---- identity/GBR branch selection -----------------------------------

    #[test]
    fn identity_matrix_selects_the_gbr_branch_and_uses_only_luma_scaling() {
        let identity = contract(
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::ColorRange::Full,
            arcen_media::BitDepth::Ten,
            arcen_media::ColorMatrix::Identity,
        );
        let u = VideoUniform::from_contract(identity, (100, 100), (100, 100));
        assert_eq!(u.mode, ShaderMode::IdentityGbr as u32);
        // Kr/Kb are meaningless for identity but must still be finite/safe.
        assert!(u.kg_inv.is_finite() && u.kg_inv != 0.0);
    }

    #[test]
    fn non_identity_matrices_select_the_matrix_branch() {
        for matrix in [
            arcen_media::ColorMatrix::Bt709,
            arcen_media::ColorMatrix::Bt601,
            arcen_media::ColorMatrix::Bt2020Ncl,
        ] {
            let c = contract(
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::ColorRange::Full,
                arcen_media::BitDepth::Eight,
                matrix,
            );
            let u = VideoUniform::from_contract(c, (10, 10), (10, 10));
            assert_eq!(u.mode, ShaderMode::Matrix as u32, "{matrix:?}");
        }
    }

    #[test]
    fn packed_rgba_passthrough_always_selects_the_passthrough_mode() {
        let u = VideoUniform::passthrough(1920, 1080);
        assert_eq!(u.mode, ShaderMode::PackedRgba as u32);
        assert_eq!(u.luma_width, 1920.0);
        assert_eq!(u.luma_height, 1080.0);
        assert_eq!(u.chroma_width, 1920.0);
        assert_eq!(u.chroma_height, 1080.0);
    }

    // ---- chroma/luma plane dimensions carried through --------------------

    #[test]
    fn subsampled_chroma_dimensions_are_carried_through_independently_of_luma() {
        let c = contract(
            arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::ColorRange::Limited,
            arcen_media::BitDepth::Eight,
            arcen_media::ColorMatrix::Bt709,
        );
        let u = VideoUniform::from_contract(c, (1920, 1080), (960, 540));
        assert_eq!(u.luma_width, 1920.0);
        assert_eq!(u.luma_height, 1080.0);
        assert_eq!(u.chroma_width, 960.0);
        assert_eq!(u.chroma_height, 540.0);
    }

    // ---- uniform byte layout ----------------------------------------------

    #[test]
    fn to_bytes_is_48_bytes_and_round_trips_every_field_in_declaration_order() {
        let u = VideoUniform::from_contract(
            contract(
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::ColorRange::Full,
                arcen_media::BitDepth::Ten,
                arcen_media::ColorMatrix::Bt2020Ncl,
            ),
            (3840, 2160),
            (3840, 2160),
        );
        let bytes = u.to_bytes();
        assert_eq!(bytes.len(), 48);
        let f32_at = |index: usize| -> f32 {
            f32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
        };
        assert_eq!(f32_at(0), u.luma_offset);
        assert_eq!(f32_at(1), u.luma_span_inv);
        assert_eq!(f32_at(2), u.chroma_center);
        assert_eq!(f32_at(3), u.chroma_span_inv);
        assert_eq!(f32_at(4), u.kr);
        assert_eq!(f32_at(5), u.kb);
        assert_eq!(f32_at(6), u.kg_inv);
        let mode = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
        assert_eq!(mode, u.mode);
        assert_eq!(f32_at(8), u.luma_width);
        assert_eq!(f32_at(9), u.luma_height);
        assert_eq!(f32_at(10), u.chroma_width);
        assert_eq!(f32_at(11), u.chroma_height);
    }

    // ---- plane descriptor construction -------------------------------------

    #[test]
    fn plane_buffer_new_accepts_an_exactly_sized_luma_plane() {
        let texels = vec![0u16; 4 * 4];
        let plane = PlaneBuffer::new(4, 4, 1, texels).expect("exact size must be accepted");
        assert_eq!(plane.width, 4);
        assert_eq!(plane.height, 4);
        assert_eq!(plane.components, 1);
        assert_eq!(plane.texels.len(), 16);
    }

    #[test]
    fn plane_buffer_new_accepts_an_exactly_sized_interleaved_chroma_plane() {
        let texels = vec![0u16; 4 * 4 * 2];
        let plane = PlaneBuffer::new(4, 4, 2, texels).expect("exact size must be accepted");
        assert_eq!(plane.components, 2);
        assert_eq!(plane.texels.len(), 32);
    }

    #[test]
    fn plane_buffer_new_rejects_a_short_buffer() {
        let texels = vec![0u16; 4 * 4 - 1];
        let error = PlaneBuffer::new(4, 4, 1, texels).unwrap_err();
        assert!(matches!(
            error,
            PlaneBufferError::LengthMismatch {
                expected: 16,
                actual: 15,
                ..
            }
        ));
    }

    #[test]
    fn plane_buffer_new_rejects_a_long_buffer() {
        let texels = vec![0u16; 4 * 4 + 1];
        let error = PlaneBuffer::new(4, 4, 1, texels).unwrap_err();
        assert!(matches!(
            error,
            PlaneBufferError::LengthMismatch {
                expected: 16,
                actual: 17,
                ..
            }
        ));
    }

    #[test]
    fn plane_buffer_new_rejects_a_zero_dimension() {
        assert!(matches!(
            PlaneBuffer::new(0, 4, 1, Vec::new()),
            Err(PlaneBufferError::ZeroDimension {
                width: 0,
                height: 4
            })
        ));
        assert!(matches!(
            PlaneBuffer::new(4, 0, 1, Vec::new()),
            Err(PlaneBufferError::ZeroDimension {
                width: 4,
                height: 0
            })
        ));
    }

    // ---- RawVideoPayload / RemoteVideoFrame --------------------------------

    #[test]
    fn from_decoded_reports_the_source_frames_own_dimensions() {
        let frame = DecodedVideoFrame {
            width: 1920,
            height: 1080,
            rgba: vec![0u8; 1920 * 1080 * 4],
            timestamp_ms: 0,
            pixel_format: "RGBA-direct".to_string(),
            backend: "test",
            native: None,
        };
        let remote = RemoteVideoFrame::from_decoded(frame);
        assert_eq!(remote.size(), (1920, 1080));
    }

    #[test]
    fn from_decoded_always_selects_the_packed_rgba_passthrough_payload() {
        let frame = DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: vec![0u8; 2 * 2 * 4],
            timestamp_ms: 0,
            pixel_format: "RGBA-direct".to_string(),
            backend: "test",
            native: None,
        };
        let remote = RemoteVideoFrame::from_decoded(frame);
        assert!(matches!(
            *remote.payload,
            RawVideoPayload::PackedRgba8 { .. }
        ));
        assert_eq!(remote.payload.uniform().mode, ShaderMode::PackedRgba as u32);
    }

    #[test]
    fn each_remote_video_frame_gets_a_distinct_monotonic_sequence() {
        let make = || {
            RemoteVideoFrame::from_decoded(DecodedVideoFrame {
                width: 1,
                height: 1,
                rgba: vec![0u8; 4],
                timestamp_ms: 0,
                pixel_format: "RGBA-direct".to_string(),
                backend: "test",
                native: None,
            })
        };
        let first = make();
        let second = make();
        assert_ne!(first.sequence, second.sequence);
        assert!(second.sequence > first.sequence);
    }

    #[test]
    fn from_planar16_carries_the_contract_and_plane_dimensions_through() {
        let luma = PlaneBuffer::new(4, 4, 1, vec![512u16; 16]).unwrap();
        let chroma = PlaneBuffer::new(4, 4, 2, vec![512u16; 32]).unwrap();
        let contract = contract(
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::ColorRange::Full,
            arcen_media::BitDepth::Ten,
            arcen_media::ColorMatrix::Identity,
        );
        let remote = RemoteVideoFrame::from_planar16(luma, chroma, contract);
        assert_eq!(remote.size(), (4, 4));
        assert_eq!(
            remote.payload.uniform().mode,
            ShaderMode::IdentityGbr as u32
        );
    }

    // ---- presentation surface format selection: still Bgra8Unorm --------

    /// Re-derives `egui_wgpu::preferred_framebuffer_format`'s exact scan
    /// order (`egui-wgpu-0.35.0/src/lib.rs`) as a local, test-only mirror --
    /// not called by any production code here (this module cannot reach or
    /// override the real one; see the module doc) -- so the "`Bgra8Unorm`
    /// always wins" claim is an executable regression, not only a citation:
    /// if a future `egui-wgpu` upgrade ever changes this scan order, this
    /// test -- not just the module doc's prose -- will start failing.
    fn mirrored_preferred_framebuffer_format(
        formats: &[wgpu::TextureFormat],
    ) -> Option<wgpu::TextureFormat> {
        formats
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
                )
            })
            .or_else(|| formats.first().copied())
    }

    #[test]
    fn presentation_format_selection_prefers_bgra8unorm_even_when_10bit_is_supported() {
        // The exact list `wgpu-hal-29.0.4/src/metal/adapter.rs`'s
        // `surface_capabilities()` builds when the Metal device *does*
        // support a 10-bit swapchain (`format_rgb10a2_unorm_all`):
        // `Bgra8Unorm` is always pushed first, unconditionally.
        let formats_with_10bit_support = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureFormat::Rgb10a2Unorm,
        ];
        assert_eq!(
            mirrored_preferred_framebuffer_format(&formats_with_10bit_support),
            Some(wgpu::TextureFormat::Bgra8Unorm),
            "Bgra8Unorm must win even though Rgb10a2Unorm is present and supported"
        );

        // And without 10-bit support at all (the capability bit unset):
        let formats_without_10bit_support = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba16Float,
        ];
        assert_eq!(
            mirrored_preferred_framebuffer_format(&formats_without_10bit_support),
            Some(wgpu::TextureFormat::Bgra8Unorm)
        );
    }

    #[test]
    fn presentation_format_selection_falls_back_to_the_first_format_when_none_preferred() {
        let exotic_only = [
            wgpu::TextureFormat::Rgb10a2Unorm,
            wgpu::TextureFormat::Rgba16Float,
        ];
        assert_eq!(
            mirrored_preferred_framebuffer_format(&exotic_only),
            Some(wgpu::TextureFormat::Rgb10a2Unorm),
            "falls back to the first format when neither Rgba8Unorm nor Bgra8Unorm is present"
        );
    }

    // ---- ReferenceColorSpace choice ---------------------------------------

    #[test]
    fn reference_colorspace_defaults_to_srgb_when_no_contract_is_known() {
        // Today's only reachable case: `RawVideoPayload::PackedRgba8` carries
        // no `VideoColorContract` at all.
        assert_eq!(reference_colorspace_for(None), ReferenceColorSpace::Srgb);
    }

    // ---- PresentationColorSpace: the HDR decision -------------------------

    /// **The axis that decides HDR is `transfer`, and only `transfer`.**
    ///
    /// This is the single most load-bearing test in the presentation path.
    /// Every other axis has, at some point, looked like a reasonable proxy
    /// for "is this HDR", and each one is wrong:
    ///
    /// - `depth`: Grading Reference is 4:4:4 ten-bit BT.709 and entirely
    ///   SDR. Keying on depth turns EDR on for a colour-critical SDR
    ///   session and tone-maps it against a 1000-nit curve.
    /// - `primaries`: BT.2020 with a BT.709 transfer is a wide-gamut SDR
    ///   signal, which is a real thing a host can send.
    /// - `matrix`: BT.2020 NCL says nothing about dynamic range either.
    #[test]
    fn only_the_pq_transfer_selects_hdr_presentation() {
        for primaries in [
            arcen_media::ColorPrimaries::Bt709,
            arcen_media::ColorPrimaries::DisplayP3,
            arcen_media::ColorPrimaries::Bt2020,
        ] {
            assert_eq!(
                presentation_colorspace_for(primaries, arcen_media::TransferCharacteristics::Pq),
                PresentationColorSpace::Hdr10Pq,
                "PQ must select HDR regardless of primaries ({primaries:?})"
            );
            for sdr in [
                arcen_media::TransferCharacteristics::Bt709,
                arcen_media::TransferCharacteristics::Srgb,
            ] {
                assert_eq!(
                    presentation_colorspace_for(primaries, sdr),
                    PresentationColorSpace::Sdr(reference_colorspace_for(Some(primaries))),
                    "{sdr:?} must stay SDR even with {primaries:?} primaries"
                );
            }
        }
    }

    /// Wide-gamut BT.2020 on an SDR curve keeps its wide working space but
    /// must not claim extended dynamic range.
    #[test]
    fn bt2020_with_an_sdr_transfer_is_wide_gamut_sdr_not_hdr() {
        assert_eq!(
            presentation_colorspace_for(
                arcen_media::ColorPrimaries::Bt2020,
                arcen_media::TransferCharacteristics::Bt709,
            ),
            PresentationColorSpace::Sdr(ReferenceColorSpace::DisplayP3)
        );
    }

    /// HLG is HDR, but it is not the PQ path this layer implements
    /// (`CAEDRMetadata::HLGMetadata` and a different colour space). Until
    /// that is built and verified on hardware, HLG must fall back to SDR
    /// rather than being presented through PQ machinery that would
    /// misinterpret its codes.
    #[test]
    fn hlg_is_not_silently_presented_as_pq() {
        assert_eq!(
            presentation_colorspace_for(
                arcen_media::ColorPrimaries::Bt2020,
                arcen_media::TransferCharacteristics::Hlg,
            ),
            PresentationColorSpace::Sdr(ReferenceColorSpace::DisplayP3)
        );
    }

    /// The eight-bit `egui`/`wgpu` swapchain's own chooser has no HDR
    /// variant in its vocabulary at all, so it cannot be told to tone-map
    /// the UI even by a caller that wanted to.
    #[test]
    fn the_eight_bit_surfaces_chooser_can_never_return_hdr() {
        for primaries in [
            arcen_media::ColorPrimaries::Bt709,
            arcen_media::ColorPrimaries::DisplayP3,
            arcen_media::ColorPrimaries::Bt2020,
        ] {
            assert!(matches!(
                reference_colorspace_for(Some(primaries)),
                ReferenceColorSpace::Srgb | ReferenceColorSpace::DisplayP3
            ));
        }
    }

    #[test]
    fn reference_colorspace_is_srgb_for_bt709() {
        assert_eq!(
            reference_colorspace_for(Some(arcen_media::ColorPrimaries::Bt709)),
            ReferenceColorSpace::Srgb
        );
    }

    #[test]
    fn reference_colorspace_is_display_p3_for_display_p3_and_bt2020() {
        for primaries in [
            arcen_media::ColorPrimaries::DisplayP3,
            arcen_media::ColorPrimaries::Bt2020,
        ] {
            assert_eq!(
                reference_colorspace_for(Some(primaries)),
                ReferenceColorSpace::DisplayP3,
                "{primaries:?}"
            );
        }
    }

    #[test]
    fn raw_video_payload_primaries_is_none_for_packed_rgba8() {
        let payload = RawVideoPayload::PackedRgba8 {
            width: 1,
            height: 1,
            rgba: Arc::from(vec![0u8; 4]),
        };
        assert_eq!(payload.primaries(), None);
    }

    #[test]
    fn raw_video_payload_primaries_is_the_contracts_primaries_for_planar16() {
        let luma = PlaneBuffer::new(1, 1, 1, vec![0u16]).unwrap();
        let chroma = PlaneBuffer::new(1, 1, 2, vec![0u16, 0u16]).unwrap();
        let contract = VideoColorContract {
            chroma: arcen_media::ChromaSubsampling::Yuv444,
            range: arcen_media::ColorRange::Full,
            depth: arcen_media::BitDepth::Ten,
            matrix: arcen_media::ColorMatrix::Bt2020Ncl,
            primaries: arcen_media::ColorPrimaries::DisplayP3,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        };
        let payload = RawVideoPayload::Planar16 {
            luma,
            chroma,
            contract,
        };
        assert_eq!(
            payload.primaries(),
            Some(arcen_media::ColorPrimaries::DisplayP3)
        );
    }

    // ---- ColorspaceApplication: the fallback/retry/log-once decision -----

    #[test]
    fn colorspace_application_needs_attempt_before_anything_is_applied() {
        let state = ColorspaceApplication::default();
        assert!(state.needs_attempt(ReferenceColorSpace::Srgb));
        assert!(state.needs_attempt(ReferenceColorSpace::DisplayP3));
    }

    #[test]
    fn colorspace_application_stops_needing_attempts_once_applied() {
        let mut state = ColorspaceApplication::default();
        state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::Applied);
        assert!(!state.needs_attempt(ReferenceColorSpace::Srgb));
        // A different desired colour space still needs its own attempt.
        assert!(state.needs_attempt(ReferenceColorSpace::DisplayP3));
    }

    #[test]
    fn colorspace_application_keeps_needing_attempts_after_a_failure() {
        let mut state = ColorspaceApplication::default();
        state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoRootWindow);
        assert!(
            state.needs_attempt(ReferenceColorSpace::Srgb),
            "a failure must retry on the next frame, not be treated as final"
        );
    }

    #[test]
    fn colorspace_application_logs_the_first_attempt_regardless_of_outcome() {
        let mut state = ColorspaceApplication::default();
        assert_eq!(
            state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoRootWindow),
            Some(ColorspaceOutcome::NoRootWindow)
        );
    }

    #[test]
    fn colorspace_application_does_not_repeat_an_identical_failure_every_frame() {
        let mut state = ColorspaceApplication::default();
        state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoRootWindow);
        assert_eq!(
            state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoRootWindow),
            None,
            "an identical repeat failure must not be logged again"
        );
    }

    #[test]
    fn colorspace_application_logs_a_change_from_one_failure_reason_to_another() {
        let mut state = ColorspaceApplication::default();
        state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoRootWindow);
        assert_eq!(
            state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoContentView),
            Some(ColorspaceOutcome::NoContentView)
        );
    }

    #[test]
    fn colorspace_application_logs_eventual_success_after_earlier_failures() {
        let mut state = ColorspaceApplication::default();
        state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::NoRootWindow);
        assert_eq!(
            state.record(ReferenceColorSpace::Srgb, ColorspaceOutcome::Applied),
            Some(ColorspaceOutcome::Applied)
        );
        assert!(!state.needs_attempt(ReferenceColorSpace::Srgb));
    }
}
