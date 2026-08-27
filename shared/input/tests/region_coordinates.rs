use arcen_input::{CoordinateTransformError, RegionCoordinateTransformer};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionDescriptor, AppliedRegionSet, LogicalPoint,
    LogicalRect, LogicalSize, OutputIdentity, OutputTransform, PhysicalSize, RegionDescriptor,
    RegionGeneration, RegionId, Scale120,
};

fn applied_region(
    id: u32,
    scale: u32,
    transform: OutputTransform,
    origin: AppliedPoint,
    primary: bool,
) -> AppliedRegionDescriptor {
    let descriptor = RegionDescriptor::new(
        RegionId::new(id).unwrap(),
        OutputIdentity::new(format!("deck-{id}")).unwrap(),
        LogicalRect::new(
            LogicalPoint::from_pixels(-30, 20).unwrap(),
            LogicalSize::from_pixels(8, 12).unwrap(),
        )
        .unwrap(),
        PhysicalSize::new(13, 17).unwrap(),
        Scale120::new(scale).unwrap(),
        transform,
        primary,
    );
    let rect = AppliedRect::new(origin, descriptor.expected_applied_size().unwrap()).unwrap();
    AppliedRegionDescriptor::new(descriptor, rect).unwrap()
}

#[test]
fn rotations_reflections_negative_origins_scales_endpoints_and_round_trips() {
    let scales = [120, 150, 180, 240];
    let origins = [
        AppliedPoint::new(-100, -75),
        AppliedPoint::new(0, 0),
        AppliedPoint::new(41, -23),
    ];

    for transform in OutputTransform::ALL {
        for scale in scales {
            for origin in origins {
                let region = applied_region(1, scale, transform, origin, true);
                let set =
                    AppliedRegionSet::new(RegionGeneration::new(7).unwrap(), vec![region.clone()])
                        .unwrap();
                let service = RegionCoordinateTransformer::new(&set);
                let logical_size = region.descriptor().logical_rect().size();
                let last_logical = LogicalPoint::new(
                    logical_size.width() as i64 - 1,
                    logical_size.height() as i64 - 1,
                );
                let applied_start = service
                    .logical_to_applied(region.id(), LogicalPoint::new(0, 0))
                    .unwrap();
                let applied_end = service
                    .logical_to_applied(region.id(), last_logical)
                    .unwrap();
                assert!(region.applied_rect().contains(applied_start));
                assert!(region.applied_rect().contains(applied_end));

                for y in 0..logical_size.height() {
                    for x in [0, 1, logical_size.width() / 2, logical_size.width() - 1] {
                        let point = LogicalPoint::new(x as i64, y as i64);
                        let applied = service.logical_to_applied(region.id(), point).unwrap();
                        let logical = service.applied_to_logical(region.id(), applied).unwrap();
                        let remapped = service.logical_to_applied(region.id(), logical).unwrap();
                        assert!((remapped.x - applied.x).abs() <= 1);
                        assert!((remapped.y - applied.y).abs() <= 1);
                    }
                }

                let rect = region.applied_rect();
                for y in 0..rect.size().height() {
                    for x in 0..rect.size().width() {
                        let applied = AppliedPoint::new(
                            rect.origin().x + i64::from(x),
                            rect.origin().y + i64::from(y),
                        );
                        let logical = service.applied_to_logical(region.id(), applied).unwrap();
                        assert_eq!(
                            service.logical_to_applied(region.id(), logical).unwrap(),
                            applied,
                            "{transform:?} scale={scale} origin={origin:?} applied={applied:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn explicit_physical_size_not_scale_controls_pixel_mapping() {
    let first = applied_region(
        11,
        120,
        OutputTransform::Normal,
        AppliedPoint::new(-20, 5),
        true,
    );
    let second = applied_region(
        22,
        240,
        OutputTransform::Rotate270,
        AppliedPoint::new(300, -40),
        false,
    );
    let set = AppliedRegionSet::new(
        RegionGeneration::new(99).unwrap(),
        vec![first.clone(), second.clone()],
    )
    .unwrap();
    let service = RegionCoordinateTransformer::new(&set);

    assert_eq!(set.generation().get(), 99);
    assert_eq!(set.primary().id(), first.id());
    assert_eq!(
        first.descriptor().physical_size(),
        second.descriptor().physical_size()
    );
    assert_eq!(
        service
            .logical_to_applied(first.id(), LogicalPoint::new(0, 0))
            .unwrap(),
        first.applied_rect().origin()
    );
    assert_ne!(
        service
            .logical_to_applied(second.id(), LogicalPoint::new(0, 0))
            .unwrap(),
        second.applied_rect().origin()
    );
}

#[test]
fn exclusive_logical_and_applied_extents_fail_closed() {
    let region = applied_region(
        1,
        120,
        OutputTransform::Normal,
        AppliedPoint::new(-10, -10),
        true,
    );
    let set =
        AppliedRegionSet::new(RegionGeneration::new(1).unwrap(), vec![region.clone()]).unwrap();
    let service = RegionCoordinateTransformer::new(&set);
    let logical_size = region.descriptor().logical_rect().size();

    assert_eq!(
        service.logical_to_applied(
            region.id(),
            LogicalPoint::new(logical_size.width() as i64, 0)
        ),
        Err(CoordinateTransformError::LogicalPointOutsideRegion)
    );
    assert_eq!(
        service.applied_to_logical(
            region.id(),
            AppliedPoint::new(
                region.applied_rect().origin().x + i64::from(region.applied_rect().size().width()),
                region.applied_rect().origin().y,
            )
        ),
        Err(CoordinateTransformError::AppliedPointOutsideRegion)
    );
    assert_eq!(
        service.logical_to_applied(RegionId::new(33).unwrap(), LogicalPoint::new(0, 0)),
        Err(CoordinateTransformError::UnknownRegion(
            RegionId::new(33).unwrap()
        ))
    );
}

/// Reproduces the host's `map_send_input_axis`: a truncating map of a signed
/// virtual-desktop pixel onto the 0..=65535 absolute axis SendInput consumes.
fn send_input_axis(coordinate: i64, origin: i64, extent: i64) -> i64 {
    assert!(extent > 1);
    let offset = coordinate - origin;
    assert!((0..extent).contains(&offset));
    offset * 65_535 / (extent - 1)
}

fn windows_pier_region(
    id: u32,
    origin_px: (i64, i64),
    size_px: (u64, u64),
    primary: bool,
) -> AppliedRegionDescriptor {
    let descriptor = RegionDescriptor::new(
        RegionId::new(id).unwrap(),
        OutputIdentity::new(format!("windows_pier-{id}")).unwrap(),
        LogicalRect::new(
            LogicalPoint::from_pixels(origin_px.0, origin_px.1).unwrap(),
            LogicalSize::from_pixels(size_px.0, size_px.1).unwrap(),
        )
        .unwrap(),
        PhysicalSize::new(size_px.0 as u32, size_px.1 as u32).unwrap(),
        Scale120::new(120).unwrap(),
        OutputTransform::Normal,
        primary,
    );
    let rect = AppliedRect::new(
        AppliedPoint::new(origin_px.0, origin_px.1),
        descriptor.expected_applied_size().unwrap(),
    )
    .unwrap();
    AppliedRegionDescriptor::new(descriptor, rect).unwrap()
}

/// Regression pin for the a Windows Pier "cursor lands on the wrong display"
/// investigation: a 3008x1692 primary at the desktop origin plus a 1800x1130
/// secondary placed left of and below it, so the secondary occupies negative
/// desktop X. The three samples below are the literal `region_pointer_move`
/// records captured from the host's own level-3 session log. Reproducing them
/// exactly is what ruled out the shared transform, the applied desktop
/// origin/scale, and a stale deployed host binary as causes of that report,
/// leaving a client-side stale secondary-viewport sample as the explanation.
#[test]
fn windows_pier_two_display_topology_reproduces_the_captured_host_coordinates() {
    let primary = windows_pier_region(1, (0, 0), (3_008, 1_692), true);
    let secondary = windows_pier_region(2, (-1_800, 832), (1_800, 1_130), false);
    let set = AppliedRegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![primary.clone(), secondary.clone()],
    )
    .unwrap();
    let service = RegionCoordinateTransformer::new(&set);

    let (desktop_left, desktop_top) = (-1_800, 0);
    let (desktop_width, desktop_height) = (4_808, 1_962);

    for (region, other, desktop_x, desktop_y, ax, ay) in [
        (&primary, &secondary, 400, 519, 29_993, 17_344),
        (&primary, &secondary, 1_077, 160, 39_222, 5_347),
        (&secondary, &primary, -1_151, 1_712, 8_847, 57_213),
    ] {
        let applied = AppliedPoint::new(desktop_x, desktop_y);
        assert!(
            region.applied_rect().contains(applied),
            "desktop ({desktop_x}, {desktop_y}) must belong to region {:?}",
            region.id()
        );
        assert!(
            !other.applied_rect().contains(applied),
            "desktop ({desktop_x}, {desktop_y}) must not also belong to region {:?}",
            other.id()
        );

        let logical = service.applied_to_logical(region.id(), applied).unwrap();
        assert_eq!(
            service.logical_to_applied(region.id(), logical).unwrap(),
            applied,
            "the captured desktop pixel must be a stable fixed point"
        );
        assert_eq!(
            service.applied_to_logical(other.id(), applied),
            Err(CoordinateTransformError::AppliedPointOutsideRegion),
            "the sibling region must refuse a point it does not own"
        );

        assert_eq!(
            send_input_axis(applied.x, desktop_left, desktop_width),
            ax,
            "SendInput x for desktop ({desktop_x}, {desktop_y})"
        );
        assert_eq!(
            send_input_axis(applied.y, desktop_top, desktop_height),
            ay,
            "SendInput y for desktop ({desktop_x}, {desktop_y})"
        );
    }
}

/// The secondary spans its whole rect entirely in negative desktop X and never
/// overlaps the primary, so a sample that belongs to one display can never be
/// mistaken for the other by the transform alone.
#[test]
fn windows_pier_secondary_never_overlaps_the_primary_across_its_whole_rect() {
    let primary = windows_pier_region(1, (0, 0), (3_008, 1_692), true);
    let secondary = windows_pier_region(2, (-1_800, 832), (1_800, 1_130), false);
    let set = AppliedRegionSet::new(
        RegionGeneration::new(7).unwrap(),
        vec![primary.clone(), secondary.clone()],
    )
    .unwrap();
    let service = RegionCoordinateTransformer::new(&set);

    for local_x in [0, 1, 900, 1_798, 1_799] {
        for local_y in [0, 1, 565, 1_128, 1_129] {
            let applied = service
                .logical_to_applied(
                    secondary.id(),
                    LogicalPoint::from_pixels(local_x, local_y).unwrap(),
                )
                .unwrap();
            assert!(
                secondary.applied_rect().contains(applied),
                "({local_x}, {local_y}) escaped its own region"
            );
            assert!(
                !primary.applied_rect().contains(applied),
                "({local_x}, {local_y}) leaked into the primary"
            );
            assert!(applied.x < primary.applied_rect().origin().x);
            assert!(
                applied.x < 0,
                "the whole secondary lives in negative desktop X"
            );
        }
    }

    assert_eq!(
        service.logical_to_applied(secondary.id(), LogicalPoint::from_pixels(1_800, 0).unwrap()),
        Err(CoordinateTransformError::LogicalPointOutsideRegion),
        "a logical point past the secondary's exclusive extent must fail closed"
    );
}
