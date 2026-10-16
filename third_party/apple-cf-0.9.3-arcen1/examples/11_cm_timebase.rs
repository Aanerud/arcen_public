use apple_cf::cm::{CMClock, CMTime, CMTimeRange, CMTimebase};

fn main() {
    let range = CMTimeRange::new(CMTime::new(0, 600), CMTime::new(300, 600));
    assert_eq!(range.end(), CMTime::new(300, 600));
    assert!(range.contains_time(CMTime::new(150, 600)));

    let clock = CMClock::host_time_clock();
    let timebase = CMTimebase::with_master_clock(&clock).expect("timebase");
    assert_eq!(timebase.set_rate(1.0), 0);
    assert_eq!(timebase.set_time(CMTime::new(0, 600)), 0);
    assert!(timebase.time().is_valid());
}
