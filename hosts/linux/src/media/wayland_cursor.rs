// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Wayland cursor images from capture to the input/cursor lane.
//!
//! A headless Wayland desktop whose streams report the cursor as metadata
//! has no X cursor watcher: its capenc children report cursor changes on
//! framed-v2 stdout, each child's reader applies them to the desktop's
//! [`arcen_input::cursor::DesktopCursor`] here, and every attachment's
//! input-lane worker follows the result through a `tokio` watch channel.
//!
//! The same hub holds the desktop's pointer ([`DesktopPointer`]) for a Deck
//! that draws its cursor at host-reported positions: capture reports the
//! compositor's core pointer ([`WaylandCursorHub::apply_position`]). Mutter
//! gives a tablet tool its own position and reports only the core pointer
//! in cursor metadata, so a USB-bridged pen's position comes from its own
//! evdev node through the one Linux pen adapter
//! (`usb_bridge::tablet_pointer`, ADR 0023), into the same pointer.
//!
//! The state lives as long as the desktop, not the attachment, because
//! Mutter sends a sprite only when it changes: a capture that reconnects to
//! the same stream after a resume or a capture restart sees positions only,
//! and the remembered sprite fills in. A desktop is found by its private
//! runtime directory, which is unique to it; the Pier serves one desktop at
//! a time, so at most [`MAX_DESKTOPS`] are kept.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arcen_input::cursor::{CursorAggregateOutcome, CursorFacts, DesktopCursor};
use arcen_input::host_pointer::DesktopPointerSample;
use arcen_input::tablet_pointer::PenMapping;
use arcen_media::capenc_stream::{CapencCursorEvent, CapencCursorPosition};
use tokio::sync::watch;

use crate::logging::target;
use crate::media::desktop_pointer::DesktopPointer;

/// Desktops whose cursor state is kept at once.
pub const MAX_DESKTOPS: usize = 2;

/// How long a cursor that no output reports may wait for another output
/// of the desktop to report it before it is sent as hidden: crossing
/// outputs is reported by two captures in either order.
pub const CROSSING_GRACE: std::time::Duration = std::time::Duration::from_millis(80);

/// One desktop's cursor and its followers.
#[derive(Debug)]
pub struct WaylandCursorHub {
    runtime_dir: PathBuf,
    desktop: Mutex<DesktopCursor>,
    facts: watch::Sender<Option<CursorFacts>>,
    /// Where the pointer is: the core pointer from capture, a bridged pen
    /// from its evdev node. GNOME maps a tablet's whole area to the whole
    /// output by default (`org.gnome.desktop.peripherals.tablet` `area`,
    /// `keep-aspect`, `output`; reading a user's remap is a follow-up).
    pointer: Arc<DesktopPointer>,
}

impl WaylandCursorHub {
    /// Applies one capture event from the stream of output `connector` in
    /// output generation `generation`.
    pub fn apply(self: &Arc<Self>, connector: &str, generation: u64, event: &CapencCursorEvent) {
        let mut desktop = self
            .desktop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match desktop.apply(connector, generation, event) {
            CursorAggregateOutcome::Changed => {
                let current = desktop.current().cloned();
                tracing::debug!(
                    target: target::INPUT,
                    connector,
                    cursor = current.as_ref().map(CursorFacts::summary).as_deref(),
                    crossing = desktop.hidden_while_crossing(),
                    "Wayland cursor changed"
                );
                match tokio::runtime::Handle::try_current() {
                    Ok(runtime) if desktop.hidden_while_crossing() => {
                        let hub = Arc::clone(self);
                        runtime.spawn(async move {
                            tokio::time::sleep(CROSSING_GRACE).await;
                            hub.publish_if_current(current);
                        });
                    }
                    _ => {
                        self.facts.send_replace(current);
                    }
                }
            }
            CursorAggregateOutcome::Unchanged => {}
            CursorAggregateOutcome::ImageRefused(error) => {
                if desktop.refused_images() == 1 {
                    tracing::warn!(
                        target: target::INPUT,
                        connector,
                        %error,
                        "a Wayland cursor image could not be fitted to the wire; the Deck keeps the previous cursor"
                    );
                }
            }
        }
    }

    /// Sends `pending` if the desktop's cursor is still exactly that.
    fn publish_if_current(&self, pending: Option<CursorFacts>) {
        let desktop = self
            .desktop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if desktop.current() == pending.as_ref() {
            self.facts.send_replace(pending);
        }
    }

    /// Applies one pointer position from the stream of output `connector`,
    /// whose stream is `stream_size` pixels. Positions are in that stream's
    /// pixels, so they are published as a desktop of that size: a
    /// single-output desktop (a multi-output Wayland desktop does not report
    /// positions yet, see `server.rs`).
    pub fn apply_position(
        &self,
        connector: &str,
        stream_size: (u32, u32),
        position: &CapencCursorPosition,
    ) {
        if !position.on_output {
            return;
        }
        tracing::trace!(
            target: target::INPUT,
            connector,
            x = position.x,
            y = position.y,
            "Wayland pointer position"
        );
        self.pointer.platform(DesktopPointerSample {
            x: position.x,
            y: position.y,
            desktop_width: stream_size.0,
            desktop_height: stream_size.1,
            at: std::time::Instant::now(),
        });
    }

    /// Notes the pixel size of the stream positions are reported in.
    pub fn note_stream_size(&self, stream_size: (u32, u32)) {
        self.pointer.set_desktop_size(stream_size.0, stream_size.1);
    }

    /// The desktop's pointer, for the bridged pen's reader.
    #[must_use]
    pub fn desktop_pointer(&self) -> Arc<DesktopPointer> {
        Arc::clone(&self.pointer)
    }

    /// Follows the pointer's position.
    #[must_use]
    pub fn subscribe_pointer(&self) -> watch::Receiver<Option<DesktopPointerSample>> {
        self.pointer.subscribe()
    }

    /// Follows the current cursor; the receiver starts with the cursor as
    /// last reported, so a new attachment shows it at once.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Option<CursorFacts>> {
        self.facts.subscribe()
    }
}

/// The desktops' hubs, most recently used last.
#[derive(Debug, Default)]
pub struct HubRegistry(Mutex<Vec<Arc<WaylandCursorHub>>>);

impl HubRegistry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// The hub of the desktop whose private runtime directory is
    /// `runtime_dir`, created on first use.
    #[must_use]
    pub fn hub(&self, runtime_dir: &Path) -> Arc<WaylandCursorHub> {
        let mut hubs = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = hubs.iter().position(|hub| hub.runtime_dir == runtime_dir) {
            let hub = hubs.remove(index);
            hubs.push(Arc::clone(&hub));
            return hub;
        }
        let hub = Arc::new(WaylandCursorHub {
            runtime_dir: runtime_dir.to_path_buf(),
            desktop: Mutex::new(DesktopCursor::default()),
            facts: watch::channel(None).0,
            pointer: Arc::new(DesktopPointer::new(PenMapping::WholeArea)),
        });
        hubs.push(Arc::clone(&hub));
        while hubs.len() > MAX_DESKTOPS {
            hubs.remove(0);
        }
        hub
    }
}

static HUBS: HubRegistry = HubRegistry::new();

/// The cursor hub of the desktop whose private runtime directory is
/// `runtime_dir`, created on first use.
#[must_use]
pub fn hub(runtime_dir: &Path) -> Arc<WaylandCursorHub> {
    HUBS.hub(runtime_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::capenc_stream::CapencCursorSprite;
    use arcen_media::cursor::CursorImage;

    #[test]
    fn a_desktop_outlives_its_attachments_and_old_desktops_are_dropped() {
        let registry = HubRegistry::new();
        let dir = Path::new("/run/user/1000/arcen-wayland-test");
        let first = registry.hub(dir);
        let image =
            CursorImage::from_straight_rgba(2, 2, (0, 0), [1, 0, 0, 255].repeat(4)).expect("image");
        first.apply(
            "Meta-0",
            1,
            &CapencCursorEvent {
                on_output: true,
                sprite: CapencCursorSprite::Image(image),
            },
        );
        // A later attachment (or a restarted capture) finds the same desktop
        // and its remembered cursor.
        let again = registry.hub(dir);
        assert!(Arc::ptr_eq(&first, &again));
        assert!(matches!(
            *again.subscribe().borrow(),
            Some(CursorFacts::Image { output: 0, .. })
        ));
        for index in 0..MAX_DESKTOPS {
            let _ = registry.hub(Path::new(&format!("/run/user/1000/other-{index}")));
        }
        assert!(!Arc::ptr_eq(&first, &registry.hub(dir)));
    }

    #[tokio::test]
    async fn crossing_outputs_in_either_order_never_flashes_hidden() {
        let registry = HubRegistry::new();
        let hub = registry.hub(Path::new("/run/user/1000/arcen-wayland-cross"));
        let image =
            CursorImage::from_straight_rgba(2, 2, (0, 0), [1, 0, 0, 255].repeat(4)).expect("image");
        let event = |on_output, sprite| CapencCursorEvent { on_output, sprite };
        hub.apply("Meta-0", 1, &event(true, CapencCursorSprite::Image(image)));
        hub.apply("Meta-1", 1, &event(false, CapencCursorSprite::Unchanged));
        let mut follower = hub.subscribe();
        follower.borrow_and_update();
        // The output left reports first, then the output entered.
        hub.apply("Meta-0", 1, &event(false, CapencCursorSprite::Unchanged));
        hub.apply("Meta-1", 1, &event(true, CapencCursorSprite::Unchanged));
        assert!(matches!(
            *follower.borrow_and_update(),
            Some(CursorFacts::Image { output: 1, .. })
        ));
        tokio::time::sleep(CROSSING_GRACE * 2).await;
        assert!(!follower.has_changed().unwrap(), "no late hidden");
        // Leaving every output does hide it, after the grace.
        hub.apply("Meta-1", 1, &event(false, CapencCursorSprite::Unchanged));
        assert!(!follower.has_changed().unwrap());
        tokio::time::sleep(CROSSING_GRACE * 2).await;
        assert!(matches!(
            *follower.borrow_and_update(),
            Some(CursorFacts::Hidden { .. })
        ));
    }
}
