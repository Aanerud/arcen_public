#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameAction {
    NewFrameStaged,
    RestageLatest,
    SubmitBlank,
    SkipNoNew,
}

pub(crate) fn choose_frame_action(
    new_frame: bool,
    have_latest: bool,
    blank_allowed: bool,
) -> FrameAction {
    if new_frame {
        FrameAction::NewFrameStaged
    } else if have_latest {
        FrameAction::RestageLatest
    } else if blank_allowed {
        FrameAction::SubmitBlank
    } else {
        FrameAction::SkipNoNew
    }
}

#[cfg(test)]
mod tests {
    use super::{choose_frame_action, FrameAction};

    #[test]
    fn empty_polls_never_replay_historical_ring_slots() {
        const DEPTH: usize = 4;
        let mut slots = [Some(11u64), Some(22), Some(33), Some(44)];
        let mut write_idx = 0usize;
        let mut latest = None;

        for captured_epoch in [
            Some(100u64),
            None,
            None,
            None,
            None,
            None,
            Some(200),
            None,
            None,
            None,
            None,
            None,
        ] {
            if let Some(epoch) = captured_epoch {
                latest = Some(epoch);
            }
            match choose_frame_action(captured_epoch.is_some(), latest.is_some(), false) {
                FrameAction::NewFrameStaged | FrameAction::RestageLatest => {
                    slots[write_idx] = latest;
                }
                other => panic!("unexpected action with a retained frame: {other:?}"),
            }
            let submitted = slots[write_idx].expect("submitted frame");
            assert_eq!(submitted, latest.expect("latest frame"));
            assert!(
                !matches!(submitted, 11 | 22 | 33 | 44),
                "historical slot reappeared after ring rotation"
            );
            write_idx = (write_idx + 1) % DEPTH;
        }
    }

    #[test]
    fn skips_before_first_frame_until_blank_fallback_is_allowed() {
        assert_eq!(
            choose_frame_action(false, false, false),
            FrameAction::SkipNoNew
        );
        assert_eq!(
            choose_frame_action(false, false, true),
            FrameAction::SubmitBlank
        );
    }

    /// The capture loop polls DXGI roughly every 2 ms while the ring submits
    /// roughly every 33 ms, so a fresh frame almost always lands on a poll
    /// that is *not* the one coinciding with the encode deadline.
    ///
    /// The caller must therefore carry "published but not yet submitted"
    /// across iterations. Passing only the current poll's `new_frame` would
    /// classify nearly every real capture as a restage, which is exactly the
    /// distinction the fresh/restaged telemetry exists to measure.
    #[test]
    fn a_frame_published_before_the_deadline_is_still_a_fresh_frame() {
        let mut fresh_pending = false;
        let mut latest = false;
        let mut actions = Vec::new();

        // One capture, then empty polls, with the encode deadline arriving
        // only on some of them.
        for (captured, deadline) in [
            (true, false),
            (false, false),
            (false, false),
            (false, true),
            (false, false),
            (false, true),
        ] {
            if captured {
                latest = true;
                fresh_pending = true;
            }
            if deadline {
                actions.push(choose_frame_action(fresh_pending, latest, false));
                fresh_pending = false;
            }
        }

        assert_eq!(
            actions,
            vec![FrameAction::NewFrameStaged, FrameAction::RestageLatest],
            "a capture that arrived between deadlines was miscounted"
        );
    }

    /// Naively using only the current poll would produce two restages above,
    /// and `avg_fresh_encode_ms` would measure almost nothing.
    #[test]
    fn using_only_the_current_poll_would_lose_every_fresh_frame() {
        assert_eq!(
            choose_frame_action(false, true, false),
            FrameAction::RestageLatest
        );
        assert_eq!(
            choose_frame_action(true, true, false),
            FrameAction::NewFrameStaged
        );
    }
}
