use apple_cf::cf::{
    CFFileDescriptor, CFMessagePort, CFNotificationCenter, CFRunLoop, CFRunLoopRunResult, CFSocket,
    CFStreamPair, CFString, CFTimer,
};
use std::process;
use std::time::Duration;

#[test]
fn cf_runtime_wrappers_work() {
    let center = CFNotificationCenter::local();
    center.post(
        &CFString::new("com.doomfish.apple-cf.runtime-tests"),
        None,
        false,
    );

    let timer = CFTimer::new(Duration::from_millis(10), false);
    let run_loop = CFRunLoop::current();
    run_loop.add_timer(&timer);
    assert!(timer.is_valid());
    let result = run_loop.run_in_default_mode(Duration::from_millis(20), true);
    assert!(matches!(
        result,
        CFRunLoopRunResult::Finished
            | CFRunLoopRunResult::Stopped
            | CFRunLoopRunResult::TimedOut
            | CFRunLoopRunResult::HandledSource
    ));

    let name = format!("com.doomfish.apple-cf.echo.test.{}", process::id());
    let _local = CFMessagePort::create_echo_local(&name);
    let remote = CFMessagePort::connect_remote(&name).expect("remote port");
    let reply = remote
        .send_request(b"ping", Duration::from_millis(100))
        .expect("reply");
    assert_eq!(reply, b"ping");

    let pair = CFStreamPair::new(1024);
    assert!(pair.read.open());
    assert!(pair.write.open());
    assert_eq!(pair.write.write(b"ok").expect("write"), 2);
    let mut buffer = [0_u8; 2];
    assert_eq!(pair.read.read(&mut buffer).expect("read"), 2);
    assert_eq!(&buffer, b"ok");
    pair.read.close();
    pair.write.close();

    let socket = CFSocket::udp_ipv4().expect("socket");
    assert!(socket.is_valid());
    assert!(socket.native() >= 0);

    let fd = CFFileDescriptor::from_raw_fd(0, false).expect("file descriptor");
    assert_eq!(fd.native_descriptor(), 0);
}
