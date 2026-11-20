// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The default-output lifecycle against fakes: the decisions with a test
//! clock, and the worker with a fake backend that records every stream.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arcen_media::audio::playback_queue::{INTERLEAVED_FRAME_SAMPLES, MAX_BUFFER_MS, max_samples};

use super::*;

fn policy() -> FollowPolicy {
    FollowPolicy::default()
}

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn open_generation(follower: &mut OutputFollower<&'static str>, now: Instant) -> u64 {
    match follower.next_step(now) {
        FollowStep::Open { generation } => generation,
        other => panic!("expected an open, got {other:?}"),
    }
}

fn playing_on(
    follower: &mut OutputFollower<&'static str>,
    device: &'static str,
    now: Instant,
) -> u64 {
    let generation = open_generation(follower, now);
    assert!(follower.opened(
        generation,
        device,
        device.to_owned(),
        "48000 Hz 2 ch F32".to_owned(),
        now
    ));
    generation
}

fn release(follower: &mut OutputFollower<&'static str>, now: Instant) -> u64 {
    match follower.next_step(now) {
        FollowStep::Release { generation } => {
            follower.released(generation);
            generation
        }
        other => panic!("expected a release, got {other:?}"),
    }
}

#[test]
fn repeated_stream_errors_back_off_longer_each_time_and_never_spin() {
    let policy = policy();
    let mut follower = OutputFollower::new(policy);
    let mut now = Instant::now();
    let mut generation = playing_on(&mut follower, "speakers", now);
    let mut waits = Vec::new();
    for _ in 0..7 {
        now += ms(10);
        assert!(follower.stream_failed(generation, "device error", now));
        assert_eq!(release(&mut follower, now), generation);
        let FollowStep::Wait { .. } = follower.next_step(now) else {
            panic!("a failed stream must not reopen at once");
        };
        // Wait out the backoff in poll-sized steps, as the worker does.
        let failed_at = now;
        loop {
            match follower.next_step(now) {
                FollowStep::Wait { until } => {
                    follower.observe_default(Some(&"speakers"));
                    now = until;
                }
                FollowStep::Open { generation: next } => {
                    generation = next;
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        waits.push(now - failed_at);
        assert!(follower.opened(
            generation,
            "speakers",
            "speakers".to_owned(),
            String::new(),
            now
        ));
    }
    assert_eq!(
        waits,
        [250, 500, 1_000, 2_000, 4_000, 4_000, 4_000].map(ms),
        "a device that fails as soon as it opens backs off up to the cap"
    );
    assert_eq!(follower.status().stream_errors, 7);
    assert_eq!(follower.status().rebuilds, 7);

    // A stream that played for a while starts the backoff over.
    now += policy.stable_after;
    assert!(follower.stream_failed(generation, "device error", now));
    assert_eq!(follower.status().attempt, 1);
}

#[test]
fn a_move_the_user_made_starts_the_backoff_over() {
    let policy = policy();
    let mut follower = OutputFollower::new(policy);
    let mut now = Instant::now();
    let mut generation = playing_on(&mut follower, "usb", now);
    // A long outage climbs to the cap.
    assert!(follower.stream_failed(generation, "device not available", now));
    release(&mut follower, now);
    for _ in 0..6 {
        now += policy.retry_max;
        let attempt = open_generation(&mut follower, now);
        follower.open_failed(attempt, &OpenFailure::NoDefaultDevice, None, now);
    }
    assert_eq!(follower.status().attempt, 7);
    now += policy.retry_max;
    generation = playing_on(&mut follower, "speakers", now);
    assert_eq!(follower.status().attempt, 0);

    // Hours later the user picks earbuds that refuse their first open.
    now += Duration::from_secs(3_600);
    follower.observe_default(Some(&"earbuds"));
    assert_eq!(release(&mut follower, now), generation);
    let refused = open_generation(&mut follower, now);
    follower.open_failed(
        refused,
        &OpenFailure::Refused("still connecting".to_owned()),
        Some("earbuds"),
        now,
    );
    assert_eq!(follower.status().attempt, 1);
    assert_eq!(
        follower.next_step(now),
        FollowStep::Wait {
            until: now + policy.retry_first
        }
    );
}

#[test]
fn a_new_default_moves_the_output_at_once_and_no_default_keeps_it() {
    let mut follower = OutputFollower::new(policy());
    let now = Instant::now();
    let first = playing_on(&mut follower, "headphones", now);

    follower.observe_default(None);
    assert!(matches!(follower.next_step(now), FollowStep::Watch { .. }));
    follower.observe_default(Some(&"headphones"));
    assert!(matches!(follower.next_step(now), FollowStep::Watch { .. }));

    // The headphones were removed: the system picked the speakers.
    follower.observe_default(Some(&"speakers"));
    assert_eq!(follower.status().phase, OutputPhase::Rebuilding);
    assert_eq!(
        follower.status().reason,
        Some(RebuildReason::DefaultChanged)
    );
    assert_eq!(release(&mut follower, now), first);
    let second = playing_on(&mut follower, "speakers", now);
    assert!(second > first);
    assert_eq!(follower.status().device, "speakers");
    assert_eq!(follower.status().rebuilds, 1);
    assert_eq!(follower.status().attempt, 0);
}

#[test]
fn a_delayed_replacement_opens_as_soon_as_it_appears() {
    let policy = policy();
    let mut follower = OutputFollower::new(policy);
    let mut now = Instant::now();
    let first = playing_on(&mut follower, "usb", now);

    // The only output went away: the stream fails and there is no default.
    assert!(follower.stream_failed(first, "device not available", now));
    assert_eq!(release(&mut follower, now), first);
    now += policy.retry_first;
    let generation = open_generation(&mut follower, now);
    follower.open_failed(generation, &OpenFailure::NoDefaultDevice, None, now);
    assert_eq!(follower.status().phase, OutputPhase::Rebuilding);
    assert_eq!(
        follower.status().reason,
        Some(RebuildReason::NoDefaultDevice)
    );
    assert_eq!(follower.status().attempt, 2);
    assert_eq!(follower.status().open_failures, 1);

    // Still nothing: keep waiting.
    now += ms(100);
    follower.observe_default(None);
    assert!(matches!(follower.next_step(now), FollowStep::Wait { .. }));

    // A device appears long before the backoff ends: open it now.
    now += ms(100);
    follower.observe_default(Some(&"speakers"));
    let next = open_generation(&mut follower, now);
    assert!(follower.opened(next, "speakers", "speakers".to_owned(), String::new(), now));
    assert_eq!(follower.status().phase, OutputPhase::Playing);
    assert_eq!(follower.status().rebuilds, 1);

    // A device that keeps refusing waits out its backoff, but a different
    // default does not.
    assert!(follower.stream_failed(next, "error", now));
    assert_eq!(release(&mut follower, now), next);
    now += policy.retry_max;
    let refused = open_generation(&mut follower, now);
    follower.open_failed(
        refused,
        &OpenFailure::Refused("busy".to_owned()),
        Some("speakers"),
        now,
    );
    follower.observe_default(Some(&"speakers"));
    assert!(matches!(follower.next_step(now), FollowStep::Wait { .. }));
    follower.observe_default(Some(&"hdmi"));
    assert!(matches!(follower.next_step(now), FollowStep::Open { .. }));
}

#[test]
fn closing_while_rebuilding_never_activates_or_opens_again() {
    let mut follower = OutputFollower::new(policy());
    let now = Instant::now();
    let first = playing_on(&mut follower, "speakers", now);
    follower.observe_default(Some(&"hdmi"));
    assert_eq!(release(&mut follower, now), first);
    let in_flight = open_generation(&mut follower, now);

    // The session ends while the new device is opening.
    follower.close();
    assert!(!follower.opened(in_flight, "hdmi", "hdmi".to_owned(), String::new(), now));
    assert_eq!(
        follower.next_step(now),
        FollowStep::Release {
            generation: in_flight
        }
    );
    follower.released(in_flight);
    assert_eq!(follower.next_step(now), FollowStep::Stop);
    follower.observe_default(Some(&"speakers"));
    assert_eq!(follower.next_step(now + ms(10_000)), FollowStep::Stop);
    assert_eq!(follower.status().phase, OutputPhase::Closed);

    // Closing during a backoff stops too.
    let mut follower = OutputFollower::new(policy());
    let generation = playing_on(&mut follower, "speakers", now);
    assert!(follower.stream_failed(generation, "error", now));
    follower.close();
    assert_eq!(release(&mut follower, now), generation);
    assert_eq!(follower.next_step(now + ms(10_000)), FollowStep::Stop);
}

#[test]
fn an_error_from_a_replaced_stream_changes_nothing() {
    let mut follower = OutputFollower::new(policy());
    let now = Instant::now();
    let first = playing_on(&mut follower, "speakers", now);
    follower.observe_default(Some(&"hdmi"));
    assert_eq!(release(&mut follower, now), first);
    let second = playing_on(&mut follower, "hdmi", now);

    assert!(!follower.stream_failed(first, "late error from the old stream", now));
    assert_eq!(follower.status().phase, OutputPhase::Playing);
    assert_eq!(follower.status().stream_errors, 0);
    assert_eq!(follower.held_generation(), Some(second));
    assert!(matches!(follower.next_step(now), FollowStep::Watch { .. }));
}

#[test]
fn the_rebuild_state_is_visible_in_the_status_note() {
    let mut follower = OutputFollower::new(policy());
    let now = Instant::now();
    assert_eq!(
        follower.status().note().as_deref(),
        Some("audio output opening")
    );
    let generation = playing_on(&mut follower, "speakers", now);
    assert_eq!(follower.status().note(), None);

    assert!(follower.stream_failed(generation, "device not available", now));
    let note = follower.status().note().expect("note while rebuilding");
    assert!(note.contains("rebuilding"), "{note}");
    assert!(note.contains("stream_error"), "{note}");
    assert!(note.contains("attempt 1"), "{note}");
    assert!(note.contains("device not available"), "{note}");

    release(&mut follower, now);
    let _ = playing_on(&mut follower, "hdmi", now + ms(250));
    let note = follower.status().note().expect("note after a rebuild");
    assert!(note.contains("hdmi"), "{note}");
    assert!(note.contains("rebuilds=1"), "{note}");
    follower.close();
    assert_eq!(
        follower.status().note().as_deref(),
        Some("audio output closed")
    );
}

// --- The worker, over a fake backend --------------------------------------

#[derive(Debug, Default)]
struct World {
    default: Option<&'static str>,
    opens: usize,
    refuse_next: usize,
    live: usize,
    max_live: usize,
    events: Vec<String>,
    hold_open: Option<mpsc::Receiver<()>>,
    renderers: HashMap<u64, StreamRenderer>,
    errors: HashMap<u64, StreamErrors>,
}

type WorldCell = Arc<Mutex<World>>;

fn world(default: Option<&'static str>) -> WorldCell {
    Arc::new(Mutex::new(World {
        default,
        ..World::default()
    }))
}

#[derive(Debug)]
struct FakeBackend(WorldCell);

#[derive(Debug)]
struct FakeStream {
    world: WorldCell,
    generation: u64,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        let mut world = lock(&self.world);
        world.live -= 1;
        world.events.push(format!("drop {}", self.generation));
    }
}

impl OutputBackend for FakeBackend {
    type Device = &'static str;
    type Stream = FakeStream;
    const BACKEND: &'static str = "fake";

    fn default_device(&mut self) -> Option<&'static str> {
        lock(&self.0).default
    }

    fn open(
        &mut self,
        device: &&'static str,
        ticket: &StreamTicket,
    ) -> Result<OpenedStream<FakeStream>, String> {
        let hold = lock(&self.0).hold_open.take();
        if let Some(hold) = hold {
            let _ = hold.recv();
        }
        let mut world = lock(&self.0);
        world.opens += 1;
        if world.refuse_next > 0 {
            world.refuse_next -= 1;
            return Err("refused".to_owned());
        }
        world.live += 1;
        world.max_live = world.max_live.max(world.live);
        world
            .events
            .push(format!("open {} {device}", ticket.generation()));
        world
            .renderers
            .insert(ticket.generation(), ticket.renderer(48_000, 2)?);
        world.errors.insert(ticket.generation(), ticket.errors());
        Ok(OpenedStream {
            stream: FakeStream {
                world: Arc::clone(&self.0),
                generation: ticket.generation(),
            },
            device: (*device).to_owned(),
            format: "48000 Hz 2 ch I16".to_owned(),
        })
    }
}

fn fast_policy() -> FollowPolicy {
    FollowPolicy {
        device_poll: ms(5),
        retry_first: ms(5),
        retry_max: ms(20),
        stable_after: Duration::from_secs(60),
        close_wait: ms(200),
    }
}

fn start(world: &WorldCell) -> Result<FollowedOutput<FakeBackend>, String> {
    let backend = Arc::clone(world);
    FollowedOutput::start_with(fast_policy(), move || FakeBackend(backend))
}

fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(ms(1));
    }
}

/// Runs one device callback of stream `generation`, as its device would
/// (also after the stream was dropped: a late callback).
fn render(world: &WorldCell, generation: u64, samples: usize) -> Vec<i16> {
    let mut renderer = lock(world)
        .renderers
        .remove(&generation)
        .expect("a renderer for that stream");
    let mut out = vec![7_i16; samples];
    renderer.fill_i16(&mut out);
    lock(world).renderers.insert(generation, renderer);
    out
}

/// Fills the queue past its prebuffer target so callbacks play.
fn fill(output: &FollowedOutput<FakeBackend>, value: i16) {
    let frame = [value; INTERLEAVED_FRAME_SAMPLES];
    while output.snapshot().prebuffering {
        let _ = output.push_samples(&frame);
    }
}

#[test]
fn a_first_open_failure_is_reported_and_not_retried() {
    let world = world(None);
    let error = start(&world).expect_err("no default device");
    assert!(error.contains("No default audio output device"), "{error}");

    let world = self::world(Some("speakers"));
    lock(&world).refuse_next = 1;
    assert_eq!(start(&world).expect_err("refused"), "refused");
    std::thread::sleep(ms(30));
    assert_eq!(lock(&world).opens, 1);
}

#[test]
fn an_old_stream_callback_after_replacement_plays_nothing_and_takes_nothing() {
    let world = world(Some("speakers"));
    let output = start(&world).expect("output");
    fill(&output, 9);
    assert!(render(&world, 1, 4).iter().all(|sample| *sample == 9));

    lock(&world).default = Some("hdmi");
    eventually("the move to hdmi", || {
        output.status().phase == OutputPhase::Playing && output.status().generation == 2
    });
    assert_eq!(output.status().device, "hdmi");

    let before = output.snapshot();
    let late = render(&world, 1, 64);
    assert!(late.iter().all(|sample| *sample == 0), "old stream silent");
    let after = output.snapshot();
    assert_eq!(after.queued_ms, before.queued_ms, "nothing taken");
    assert_eq!(after.underruns, before.underruns);
    assert!(render(&world, 2, 4).iter().all(|sample| *sample == 9));
}

#[test]
fn two_streams_never_exist_at_once() {
    let world = world(Some("speakers"));
    let output = start(&world).expect("output");
    for (generation, device) in [(2, "hdmi"), (3, "usb"), (4, "speakers")] {
        lock(&world).default = Some(device);
        eventually("a move", || output.status().generation == generation);
    }
    let errors = lock(&world).errors.get(&4).cloned().expect("errors");
    errors.report("device error");
    eventually("the recovery", || output.status().generation == 5);
    drop(output);

    let world = lock(&world);
    assert_eq!(world.max_live, 1, "{:?}", world.events);
    assert_eq!(world.live, 0);
    assert_eq!(
        world.events,
        [
            "open 1 speakers",
            "drop 1",
            "open 2 hdmi",
            "drop 2",
            "open 3 usb",
            "drop 3",
            "open 4 speakers",
            "drop 4",
            "open 5 speakers",
            "drop 5",
        ]
    );
}

#[test]
fn closing_during_a_slow_open_revives_nothing_and_returns_in_bounded_time() {
    let world = world(Some("speakers"));
    let mut output = start(&world).expect("output");
    fill(&output, 9);
    let (release_open, hold) = mpsc::channel();
    lock(&world).hold_open = Some(hold);
    lock(&world).default = Some("hdmi");
    eventually("the new device's open to block", || {
        lock(&world).hold_open.is_none()
    });

    let closing = Instant::now();
    output.close();
    assert!(closing.elapsed() < Duration::from_secs(1), "bounded close");

    release_open.send(()).expect("open released");
    eventually("the late stream to be dropped", || {
        lock(&world).events.iter().any(|event| event == "drop 2")
    });
    assert!(render(&world, 2, 16).iter().all(|sample| *sample == 0));
    assert!(render(&world, 1, 16).iter().all(|sample| *sample == 0));
    std::thread::sleep(ms(50));
    let world = lock(&world);
    assert_eq!(world.opens, 2, "nothing opens after close");
    assert_eq!(world.live, 0);
    drop(world);
    assert_eq!(output.status().phase, OutputPhase::Closed);
}

#[test]
fn a_device_gap_keeps_the_queue_bounded_and_is_not_an_underrun() {
    let world = world(Some("usb"));
    let output = start(&world).expect("output");
    fill(&output, 3);
    let _ = render(&world, 1, 960);

    // The USB device is unplugged and nothing replaces it for a while.
    lock(&world).default = None;
    let errors = lock(&world).errors.get(&1).cloned().expect("errors");
    errors.report("device not available");
    eventually("the gap", || {
        output.status().phase == OutputPhase::Rebuilding && output.status().attempt >= 2
    });
    let frame = [5_i16; INTERLEAVED_FRAME_SAMPLES];
    for _ in 0..100 {
        let _ = output.push_samples(&frame);
    }
    let during = output.snapshot();
    assert_eq!(during.underruns, 0, "a gap is not an underrun");
    assert!(during.queued_ms <= MAX_BUFFER_MS, "{during:?}");

    // The speakers take over; the newest audio plays, then one underrun.
    lock(&world).default = Some("speakers");
    eventually("the speakers", || {
        output.status().phase == OutputPhase::Playing && output.status().device == "speakers"
    });
    let generation = output.status().generation;
    let played = render(&world, generation, 2);
    assert_eq!(played, [5, 5], "stale samples were trimmed away");
    let _ = render(&world, generation, max_samples());
    let after = output.snapshot();
    assert_eq!(after.underruns, 1);
    assert_eq!(after.queued_ms, 0);
    assert_eq!(output.status().stream_errors, 1);
    assert!(output.status().open_failures >= 1);
}
