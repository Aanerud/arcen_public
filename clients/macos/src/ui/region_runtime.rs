// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Region-authoritative viewport and input runtime for Deck.
//!
//! The runtime is shared (`arcen_deck_core::input::region_runtime`, used by
//! the Windows Deck too); this module re-exports it and gives it the macOS
//! projection of the applied topology (`AppliedTopologyView` for the shared
//! `arcen_deck_core::multi_window::session::ValidatedAppliedTopology`). The tests below stay here: they pin
//! the shared runtime against the macOS Deck's own topology and pointer
//! mapping.

pub use arcen_deck_core::input::region_runtime::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::PointerMotionMode;
    use crate::ui::multi_window_session::ValidatedAppliedTopology;
    use arcen_input::{
        LowLatencyMetadata, MappedRegionInput, PenTool, RegionInputEvent, RegionInputPipeline,
        RegionPointMapper,
    };
    use arcen_input::{
        PenEvent, RegionCoordinateTransformer, RegionInputState, RegionInputWireMessage,
        RegionLogicalPosition, RegionPenSample, ScrollPhase, ScrollUnit,
    };
    use arcen_media::SessionMonitorId;
    use arcen_media::{
        video::EncoderBackend, BitrateBudgetKbps, MediaStreamEpoch, Monitor, MonitorIdentity,
        OutputTransform, RegionMediaPlan, RegionMediaRoster, RequestedMonitor, TopologyGeneration,
        VideoConfiguration,
    };
    use arcen_media::{
        AppliedPoint, AppliedSize, LogicalPoint, PhysicalSize, RegionId, RequestedMonitorTopology,
    };

    use crate::protocol::messages::MultiMonitorCarrierMsg;
    use crate::ui::multi_window_session::{
        map_local_fraction_to_wire_pointer, DesktopRect, MonitorDesktopRect, ResolvedAppliedMonitor,
    };

    const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/runtime/region_input.json"
    ));

    /// Stands in for a host's native pointer mapping so the parity gate
    /// observes the shared transform result itself.
    #[derive(Debug, Clone, Copy)]
    struct IdentityMapper;

    impl RegionPointMapper for IdentityMapper {
        type Point = AppliedPoint;
        type Error = std::convert::Infallible;

        fn map_applied(&self, point: AppliedPoint) -> Result<AppliedPoint, Self::Error> {
            Ok(point)
        }
    }

    fn mapped_point(mapped: MappedRegionInput<AppliedPoint>) -> AppliedPoint {
        match mapped {
            MappedRegionInput::PointerEnter(point)
            | MappedRegionInput::PointerLeave(point)
            | MappedRegionInput::PointerMotion(point) => point,
            MappedRegionInput::PointerButton(button) => button.position,
            MappedRegionInput::PointerScroll(scroll) => scroll.position,
            MappedRegionInput::Pen(pen) => pen.position,
        }
    }

    fn media_plan(monitor_id: u16, stream_epoch: u64, width: u32, height: u32) -> RegionMediaPlan {
        RegionMediaPlan::new(
            SessionMonitorId::new(monitor_id).unwrap(),
            MediaStreamEpoch::new(stream_epoch).unwrap(),
            EncoderBackend::OpenH264,
            VideoConfiguration::legacy_h264(),
            width,
            height,
            60,
            BitrateBudgetKbps::nominal_for_geometry(width, height, 60),
        )
        .unwrap()
    }

    fn requested_monitor(
        id: u32,
        x: i32,
        y: i32,
        logical_width: u32,
        logical_height: u32,
        physical_width: u32,
        physical_height: u32,
        scale: f32,
        primary: bool,
    ) -> RequestedMonitor {
        RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: id.to_string(),
                    name: format!("Display {id}"),
                    vendor: 1,
                    model: 2,
                    serial: id,
                },
                x,
                y,
                width_px: physical_width,
                height_px: physical_height,
                scale,
                refresh_hz: 60,
                rotation: arcen_media::Rotation::Degrees0,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
                color: None,
            },
            logical_width,
            logical_height,
        )
        .unwrap()
    }

    fn mixed_runtime() -> DeckRegionRuntime {
        let primary_id = SessionMonitorId::new(1).unwrap();
        let secondary_id = SessionMonitorId::new(2).unwrap();
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor(11, 0, 0, 1800, 1169, 3600, 2338, 2.0, true),
            requested_monitor(22, -2560, 0, 2560, 1440, 2560, 1440, 1.0, false),
        ])
        .unwrap();
        let roster = RegionMediaRoster::new(vec![
            media_plan(1, 41, 3600, 2338),
            media_plan(2, 77, 2560, 1440),
        ])
        .unwrap();
        let validated = ValidatedAppliedTopology {
            generation: TopologyGeneration::new(7).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![
                ResolvedAppliedMonitor {
                    session_monitor_id: primary_id,
                    display_id: 11,
                    rect: MonitorDesktopRect {
                        x: 0,
                        y: 0,
                        width_px: 3600,
                        height_px: 2338,
                    },
                },
                ResolvedAppliedMonitor {
                    session_monitor_id: secondary_id,
                    display_id: 22,
                    rect: MonitorDesktopRect {
                        x: -2560,
                        y: 0,
                        width_px: 2560,
                        height_px: 1440,
                    },
                },
            ],
            media_roster: Box::new(roster),
            desktop: DesktopRect {
                x: -2560,
                y: 0,
                width_px: 6160,
                height_px: 2338,
            },
        };
        DeckRegionRuntime::from_validated_topology(&validated, Some(&requested)).unwrap()
    }

    fn baseline_runtime() -> DeckRegionRuntime {
        let monitor_id = SessionMonitorId::new(1).unwrap();
        let requested = RequestedMonitorTopology::new(vec![requested_monitor(
            11, 0, 0, 1_920, 1_080, 1_920, 1_080, 1.0, true,
        )])
        .unwrap();
        let validated = ValidatedAppliedTopology {
            generation: TopologyGeneration::new(7).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![ResolvedAppliedMonitor {
                session_monitor_id: monitor_id,
                display_id: 11,
                rect: MonitorDesktopRect {
                    x: 0,
                    y: 0,
                    width_px: 1_920,
                    height_px: 1_080,
                },
            }],
            media_roster: Box::new(
                RegionMediaRoster::new(vec![media_plan(1, 41, 1_920, 1_080)]).unwrap(),
            ),
            desktop: DesktopRect {
                x: 0,
                y: 0,
                width_px: 1_920,
                height_px: 1_080,
            },
        };
        DeckRegionRuntime::from_validated_topology(&validated, Some(&requested)).unwrap()
    }

    #[test]
    fn cross_component_fixture_freezes_deck_encoding_state_and_endpoint() {
        let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
        let runtime = baseline_runtime();
        let generation = runtime.generation();
        let region_id = RegionId::new(1).unwrap();
        let pointer = RegionLogicalPosition {
            region_id,
            point: LogicalPoint::new(60, 1_440),
        };
        let pen = RegionPenSample {
            position: RegionLogicalPosition {
                region_id,
                point: LogicalPoint::new(600, 900),
            },
            pressure: 0.75,
            tilt_x_degrees: -12.0,
            tilt_y_degrees: 10.0,
            rotation_degrees: 180.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 1,
        };
        let events = [
            (
                RegionInputEvent::PointerEnter {
                    generation,
                    position: RegionLogicalPosition {
                        region_id,
                        point: LogicalPoint::new(0, 0),
                    },
                    sequence: 40,
                },
                9_000,
                false,
            ),
            (
                RegionInputEvent::PointerMotion {
                    generation,
                    position: pointer,
                    sequence: 41,
                },
                9_001,
                true,
            ),
            (
                RegionInputEvent::PointerButton {
                    generation,
                    position: pointer,
                    button: 1,
                    pressed: true,
                    sequence: 42,
                },
                9_002,
                false,
            ),
            (
                RegionInputEvent::PointerScroll {
                    generation,
                    position: pointer,
                    delta_x: 120,
                    delta_y: -240,
                    unit: ScrollUnit::Point,
                    phase: ScrollPhase::Changed,
                    sequence: 43,
                },
                9_003,
                false,
            ),
            (
                RegionInputEvent::Pen {
                    generation,
                    sample: pen,
                    sequence: 44,
                },
                9_004,
                true,
            ),
            (
                RegionInputEvent::PointerLeave {
                    generation,
                    position: pointer,
                    sequence: 45,
                },
                9_005,
                false,
            ),
        ];
        let mut state = RegionInputState::new();
        let encoded = events
            .into_iter()
            .map(|(event, timestamp_ns, coalescable)| {
                state.apply(runtime.applied_regions(), event).unwrap();
                let message = RegionInputWireMessage::encode(event, timestamp_ns, coalescable);
                message.validate().unwrap();
                message.to_json_value().unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(encoded.as_slice(), baseline["events"].as_array().unwrap());
        let transformer = RegionCoordinateTransformer::new(runtime.applied_regions());
        assert_eq!(
            transformer
                .logical_to_applied(pointer.region_id, pointer.point)
                .unwrap(),
            AppliedPoint::new(
                baseline["pointer_endpoint"]["x"].as_i64().unwrap(),
                baseline["pointer_endpoint"]["y"].as_i64().unwrap(),
            )
        );
        assert_eq!(
            transformer
                .logical_to_applied(pen.position.region_id, pen.position.point)
                .unwrap(),
            AppliedPoint::new(
                baseline["pen_endpoint"]["x"].as_i64().unwrap(),
                baseline["pen_endpoint"]["y"].as_i64().unwrap(),
            )
        );
        assert_eq!(state.latest_pointer_position(), Some(pointer));
        assert_eq!(state.held_buttons().collect::<Vec<_>>(), vec![1]);
        assert_eq!(state.pen().unwrap().sample, pen);
        assert_eq!(state.last_sequence(), 45);
        assert!(!state.is_focused());
    }

    #[test]
    fn center_and_all_corners_use_one_region_coordinate_transformer() {
        let runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let region = runtime.applied_regions().get(viewport.region_id()).unwrap();
        let size = region.descriptor().logical_rect().size();

        let top_left = runtime.logical_position(viewport, (0.0, 0.0)).unwrap();
        let top_right = runtime.logical_position(viewport, (1.0, 0.0)).unwrap();
        let bottom_left = runtime.logical_position(viewport, (0.0, 1.0)).unwrap();
        let bottom_right = runtime.logical_position(viewport, (1.0, 1.0)).unwrap();
        let center = runtime.logical_position(viewport, (0.5, 0.5)).unwrap();

        assert_eq!(top_left.point, LogicalPoint::new(0, 0));
        assert_eq!(
            top_right.point,
            LogicalPoint::new(i64::try_from(size.width() - 1).unwrap(), 0)
        );
        assert_eq!(
            bottom_left.point,
            LogicalPoint::new(0, i64::try_from(size.height() - 1).unwrap())
        );
        assert_eq!(
            bottom_right.point,
            LogicalPoint::new(
                i64::try_from(size.width() - 1).unwrap(),
                i64::try_from(size.height() - 1).unwrap()
            )
        );
        assert_eq!(
            RegionCoordinateTransformer::new(runtime.applied_regions())
                .logical_to_applied(center.region_id, center.point)
                .unwrap(),
            AppliedPoint::new(1800, 1169)
        );
    }

    #[test]
    fn context_menu_button_repeats_the_latest_authoritative_motion_position() {
        let mut runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let mut sequence = 0;
        let motion = runtime
            .pointer_motion(viewport, (0.75, 0.25), &mut sequence, 10)
            .unwrap();
        let motion = motion.last().unwrap();
        let button = runtime
            .pointer_button_at_latest(3, true, &mut sequence, 11)
            .unwrap()
            .unwrap();

        assert_eq!(motion.position(), button.position());
        assert_eq!(button.button_state(), Some((3, true)));
        let motion = motion.to_json_value().unwrap();
        let button = button.to_json_value().unwrap();
        assert_eq!(motion["logical_x"], button["logical_x"]);
        assert_eq!(motion["logical_y"], button["logical_y"]);
        assert!(motion.get("server_x").is_none());
        assert!(motion.get("server_y").is_none());
        assert!(button.get("server_x").is_none());
        assert!(button.get("server_y").is_none());
    }

    #[test]
    fn negotiated_region_runtime_has_no_legacy_adapter() {
        let mut runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let mut sequence = 0;
        let messages = runtime
            .pointer_motion(viewport, (0.5, 0.5), &mut sequence, 10)
            .unwrap();

        assert!(matches!(
            runtime.adapt_legacy(messages.last().unwrap(), PointerMotionMode::Absolute),
            Err(DeckRegionRuntimeError::LegacyAdapterUnavailableForRegionSession),
        ));
    }

    /// Parity gate: everything a live Match My Layout session emits -- root
    /// and secondary motion, the cross-viewport leave/enter transition,
    /// buttons, scroll, pen, and the held-button release -- is
    /// produced by one shared emitter, is accepted in order by the shared
    /// host pipeline, and lands on exactly the applied point the one shared
    /// transformer computes. There is no second client mapping path left to
    /// diverge from this one.
    #[test]
    fn every_production_region_message_is_accepted_by_the_shared_host_pipeline_unchanged() {
        let mut runtime = mixed_runtime();
        let primary = runtime.primary_viewport();
        let secondary = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        // Deck's session-global input counter: keyboard and pen already
        // consumed 40 sequences before this pointer gesture.
        let mut sequence = 40;
        let mut emitted = Vec::new();

        emitted.extend(
            runtime
                .pointer_motion(primary, (0.25, 0.25), &mut sequence, 10)
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_sample(primary, (0.5, 0.5), &[(1, true)], &mut sequence, 11)
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_scroll(primary, (0.5, 0.5), 1, -2, &mut sequence, 12)
                .unwrap(),
        );
        // Crossing viewports must derive leave-then-enter from the same
        // emitter rather than a per-viewport mapping.
        emitted.extend(
            runtime
                .pointer_button(secondary, (0.75, 0.5), 1, false, &mut sequence, 13)
                .unwrap(),
        );
        let pen_sequence = {
            sequence = sequence.saturating_add(1);
            sequence
        };
        emitted.push(
            runtime
                .pen(
                    secondary,
                    PenEvent {
                        x: 0.25,
                        y: 0.75,
                        pressure: 0.6,
                        tilt_x_degrees: 12.0,
                        tilt_y_degrees: -8.0,
                        rotation_degrees: 30.0,
                        tool: PenTool::Tip,
                        in_proximity: true,
                        touching: true,
                        buttons: 2,
                        metadata: LowLatencyMetadata {
                            sequence: pen_sequence,
                            timestamp_ns: 14,
                            coalescable: true,
                        },
                    },
                )
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_button(secondary, (0.75, 0.5), 3, true, &mut sequence, 15)
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_button_at_latest(3, false, &mut sequence, 16)
                .unwrap(),
        );

        assert_eq!(
            emitted
                .iter()
                .map(RegionInputWireMessage::input_type)
                .collect::<Vec<_>>(),
            vec![
                "region_pointer_enter",
                "region_pointer_motion",
                "region_pointer_button",
                "region_pointer_scroll",
                "region_pointer_leave",
                "region_pointer_enter",
                "region_pointer_button",
                "region_pen_event",
                "region_pointer_button",
                "region_pointer_button",
            ],
        );

        let mut host = RegionInputPipeline::new(runtime.applied_regions().clone(), IdentityMapper);
        let transformer = RegionCoordinateTransformer::new(runtime.applied_regions());
        let mut previous_sequence = 0;
        for message in &emitted {
            let metadata = message.metadata();
            assert!(
                metadata.sequence > previous_sequence,
                "region sequences must stay strictly increasing",
            );
            previous_sequence = metadata.sequence;
            assert_eq!(
                message.position().region_generation,
                runtime.generation().get(),
            );
            let mapped = host.apply(message.as_ref()).unwrap();
            let expected = transformer
                .logical_to_applied(
                    RegionId::new(message.position().region_id).unwrap(),
                    LogicalPoint::new(message.position().logical_x, message.position().logical_y),
                )
                .unwrap();
            assert_eq!(mapped_point(mapped), expected);
        }

        // The client's session-global counter, the client emitter state, and
        // the host state all agree on the same accepted sequence.
        assert_eq!(sequence, previous_sequence);
        assert_eq!(runtime.input_state().last_sequence(), previous_sequence);
        assert_eq!(host.state().last_sequence(), previous_sequence);
        assert_eq!(
            runtime.input_state().latest_pointer_position(),
            host.state().latest_pointer_position(),
        );
        assert_eq!(
            runtime.input_state().held_buttons().collect::<Vec<_>>(),
            host.state().held_buttons().collect::<Vec<_>>(),
        );
        assert_eq!(
            runtime.input_state().is_focused(),
            host.state().is_focused()
        );
        assert_eq!(
            runtime.input_state().pen().map(|pen| pen.sample),
            host.state().pen().map(|pen| pen.sample),
        );

        runtime.release_all();
        assert!(runtime.input_state().held_buttons().next().is_none());
        assert!(!runtime.input_state().is_focused());
        assert_eq!(
            runtime.input_state().last_sequence(),
            previous_sequence,
            "release-all must never rewind the accepted sequence",
        );
    }

    /// Parity gate for the deleted live secondary affine path: the region
    /// transformer places a secondary viewport's pointer on exactly the
    /// desktop pixel `map_local_fraction_to_wire_pointer` used to derive, so
    /// removing that second mapping moved no cursor.
    #[test]
    fn secondary_region_mapping_matches_the_removed_affine_desktop_mapping() {
        let mut runtime = mixed_runtime();
        let viewport = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        let monitor_rect = MonitorDesktopRect {
            x: -2560,
            y: 0,
            width_px: 2560,
            height_px: 1440,
        };
        let desktop = DesktopRect {
            x: -2560,
            y: 0,
            width_px: 6160,
            height_px: 2338,
        };
        let transformer_regions = runtime.applied_regions().clone();
        let transformer = RegionCoordinateTransformer::new(&transformer_regions);
        let mut sequence = 0;

        for (index, fraction) in [(0.0, 0.0), (0.25, 0.75), (0.5, 0.5), (1.0, 1.0)]
            .into_iter()
            .enumerate()
        {
            let messages = runtime
                .pointer_motion(viewport, fraction, &mut sequence, 100 + index as u64)
                .unwrap();
            let position = messages.last().unwrap().position();
            let applied = transformer
                .logical_to_applied(
                    RegionId::new(position.region_id).unwrap(),
                    LogicalPoint::new(position.logical_x, position.logical_y),
                )
                .unwrap();
            let (_, _, server_x, server_y) =
                map_local_fraction_to_wire_pointer(fraction, monitor_rect, desktop)
                    .expect("the legacy affine mapping is defined for this rectangle");

            assert_eq!(
                (applied.x, applied.y),
                (i64::from(server_x), i64::from(server_y)),
                "region mapping diverged from the removed affine mapping at {fraction:?}",
            );
        }
    }

    #[test]
    fn scroll_delta_must_fit_the_host_i32_contract_before_state_or_sequence_advance() {
        let mut runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let mut sequence = 0;

        assert!(matches!(
            runtime.pointer_scroll(viewport, (0.5, 0.5), i32::MAX, 0, &mut sequence, 10),
            Err(DeckRegionRuntimeError::ScrollDeltaOverflow(_)),
        ));
        assert_eq!(sequence, 0);
        assert!(!runtime.input_state().is_focused());

        let maximum_ticks = i32::MAX / 120;
        let messages = runtime
            .pointer_scroll(viewport, (0.5, 0.5), maximum_ticks, 0, &mut sequence, 11)
            .unwrap();
        let RegionInputWireMessage::PointerScroll(message) = messages.last().unwrap() else {
            panic!("last message must be the bounded scroll");
        };
        assert_eq!(message.delta_x, i64::from(maximum_ticks) * 120);
        assert!(i32::try_from(message.delta_x).is_ok());
    }

    #[test]
    fn mixed_retina_and_non_retina_sizes_keep_logical_and_media_facts_distinct() {
        let runtime = mixed_runtime();
        let primary = runtime.requested_regions().primary();
        let secondary = runtime
            .requested_regions()
            .regions()
            .iter()
            .find(|region| !region.is_primary())
            .unwrap();

        assert_eq!(primary.scale().get(), 240);
        assert_eq!(primary.logical_rect().size().width(), 1800 * 120);
        assert_eq!(primary.physical_size().width(), 3600);
        assert_eq!(primary.physical_size().height(), 2338);
        assert_eq!(secondary.scale().get(), 120);
        assert_eq!(secondary.logical_rect().size().width(), 2560 * 120);
        assert_eq!(secondary.physical_size().width(), 2560);
    }

    #[test]
    fn deck_regions_use_already_compositor_oriented_convention() {
        let monitor_id = SessionMonitorId::new(1).unwrap();
        let requested = RequestedMonitorTopology::new(vec![RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: "33".to_owned(),
                    name: "Rotated Display".to_owned(),
                    vendor: 1,
                    model: 2,
                    serial: 33,
                },
                x: 0,
                y: 0,
                width_px: 1920,
                height_px: 1080,
                scale: 1.0,
                refresh_hz: 60,
                rotation: arcen_media::Rotation::Degrees90,
                primary: true,
                width_mm: 0.0,
                height_mm: 0.0,
                color: None,
            },
            1080,
            1920,
        )
        .unwrap()])
        .unwrap();
        let validated = ValidatedAppliedTopology {
            generation: TopologyGeneration::new(8).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![ResolvedAppliedMonitor {
                session_monitor_id: monitor_id,
                display_id: 33,
                rect: MonitorDesktopRect {
                    x: 0,
                    y: 0,
                    width_px: 1080,
                    height_px: 1920,
                },
            }],
            media_roster: Box::new(
                RegionMediaRoster::new(vec![media_plan(1, 88, 1080, 1920)]).unwrap(),
            ),
            desktop: DesktopRect {
                x: 0,
                y: 0,
                width_px: 1080,
                height_px: 1920,
            },
        };
        let runtime =
            DeckRegionRuntime::from_validated_topology(&validated, Some(&requested)).unwrap();
        let region = runtime.requested_regions().primary();

        // Deck AlreadyCompositorOriented: the applied roster/media extent
        // has already absorbed the host transform, so legacy panel rotation
        // metadata must not rotate the stream a second time.
        assert_eq!(region.transform(), OutputTransform::Normal);
        assert_eq!(
            region.physical_size(),
            PhysicalSize::new(1080, 1920).unwrap()
        );
        assert_eq!(
            region.expected_applied_size().unwrap(),
            AppliedSize::new(1080, 1920).unwrap()
        );
        assert_eq!(
            runtime.applied_regions().primary().applied_rect().size(),
            AppliedSize::new(1080, 1920).unwrap()
        );
    }

    #[test]
    fn negative_layout_maps_secondary_corners_without_root_space_divergence() {
        let mut runtime = mixed_runtime();
        let viewport = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        let mut sequence = 0;
        let top_left = runtime
            .pointer_motion(viewport, (0.0, 0.0), &mut sequence, 10)
            .unwrap();
        let top_left = top_left.last().unwrap();
        let top_left_position = top_left.position();
        let top_left_wire = top_left.to_json_value().unwrap();
        let bottom_right = runtime
            .pointer_motion(viewport, (1.0, 1.0), &mut sequence, 11)
            .unwrap();
        let bottom_right = bottom_right.last().unwrap();
        let bottom_right_position = bottom_right.position();
        let bottom_right_wire = bottom_right.to_json_value().unwrap();
        let transformer = RegionCoordinateTransformer::new(runtime.applied_regions());

        assert_eq!(
            transformer
                .logical_to_applied(
                    RegionId::new(top_left_position.region_id).unwrap(),
                    LogicalPoint::new(top_left_position.logical_x, top_left_position.logical_y),
                )
                .unwrap(),
            AppliedPoint::new(-2560, 0),
        );
        assert_eq!(
            transformer
                .logical_to_applied(
                    RegionId::new(bottom_right_position.region_id).unwrap(),
                    LogicalPoint::new(
                        bottom_right_position.logical_x,
                        bottom_right_position.logical_y,
                    ),
                )
                .unwrap(),
            AppliedPoint::new(-1, 1439),
        );
        for wire in [top_left_wire, bottom_right_wire] {
            assert_eq!(wire["region_id"], 2);
            assert!(wire.get("server_x").is_none());
            assert!(wire.get("server_y").is_none());
        }
    }

    #[test]
    fn pen_sample_routes_through_its_secondary_region() {
        let mut runtime = mixed_runtime();
        let viewport = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        let event = PenEvent {
            x: 0.25,
            y: 0.75,
            pressure: 0.6,
            tilt_x_degrees: 12.0,
            tilt_y_degrees: -8.0,
            rotation_degrees: 30.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 2,
            metadata: LowLatencyMetadata {
                sequence: 1,
                timestamp_ns: 99,
                coalescable: true,
            },
        };

        let message = runtime.pen(viewport, event).unwrap();
        let position = message.position();
        assert_eq!(position.region_id, 2);
        assert_eq!(
            RegionCoordinateTransformer::new(runtime.applied_regions())
                .logical_to_applied(
                    RegionId::new(position.region_id).unwrap(),
                    LogicalPoint::new(position.logical_x, position.logical_y),
                )
                .unwrap(),
            AppliedPoint::new(-1920, 1079),
        );
        let wire = message.to_json_value().unwrap();
        assert_eq!(wire["type"], "region_pen_event");
        assert_eq!(wire["region_id"], 2);
        assert!(wire.get("server_x").is_none());
        assert!(wire.get("server_y").is_none());
    }
}
