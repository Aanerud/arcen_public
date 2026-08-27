# arcen-microphone Windows driver

This directory is independently authored Arcen source for the fixed microphone-v1
kernel contract. It does not contain or derive from SysVAD, WDK samples, virtual
cable projects, third-party drivers, or proprietary SDK payloads.

The endpoint is capture-only, 48 kHz mono signed 16-bit PCM in 20 ms frames.
PortCls registers topology and WaveRT filters with one fixed capture pin. A
periodic stream DPC transfers the ten-frame nonpaged ring into the WaveRT cyclic
buffer on an interrupt-time timeline with bounded catch-up, tracks position,
signals aligned notifications, drops the oldest frame on overrun, emits exact
silence on underrun, and synchronously clears every stream buffer on stop, power
loss, cleanup, or surprise removal.

The control device is accessible only to the deterministic `ArcenPier` service
SID. It uses buffered bind/feed/stop/status IOCTLs, exact layout checks, one
owner file context, WTS/binary-SID/generation binding, and secure clearing. The
capture pin snapshots the originating create IRP's documented requestor session
and primary user SID, then reads only the matching active generation; rejected
or stale readers receive silence without draining the ring. The
Rust host owns a two-frame mailbox and overlapped-I/O worker so network/session
tasks never perform driver I/O.

`arcen-microphone.vcxproj` is an x64 WDM/PortCls project and
`arcen-microphone.inf` supports Windows 10/11 and Server 2022+. Run
`verify-driver-source.ps1` and `test-driver-portable.cmd` without a WDK;
`hosts\windows\build.cmd` adds the native WDK build when `portcls.h` is
installed. Its unsigned output is never a release artifact. The servicing
scripts require an exact reviewed-INF SYS/INF/CAT payload covered by a Microsoft
WHCP/WHQL production catalog and use documented SetupAPI, PnPUtil, and
`DiRollbackDriver` paths.

Remaining evidence includes a native WDK build, protected EV/Partner Center and
WHCP/HLK signing, HVCI and Driver Verifier, servicing-lab rollback, and physical
multi-WTS isolation.
