# arcen-iddcx Windows indirect display driver

This directory is original, independently authored Arcen source using only
Microsoft's public UMDF 2, WDF, DXGI/D3D11, and IddCx 1.4 API surface. It does
not copy or derive from Microsoft driver samples, community virtual-display
drivers, third-party source, proprietary SDK payloads, or a local reference corpus.

The driver exposes one root-enumerated indirect adapter and accepts a fixed
versioned buffered-I/O contract from Arcen Pier. One exclusive control handle
may atomically replace a topology with one through four virtual monitors. Each
monitor carries an exact mode roster, a generated 128-byte EDID, signed desktop
coordinates, rotation, physical-size hints, and a stable connector index. The
driver asks IddCx to bind swapchains to Pier's exact render-adapter LUID and
fails verification when the OS assigns another adapter.

The control handle owns the topology. Explicit rollback departs every monitor;
WDF file cleanup performs the same rollback if Pier crashes or loses the
handle. A failed replacement removes its partial arrivals and recreates the
previous descriptor set. Adapter stop and device cleanup also depart all
monitors. No physical display state or Arcen display-recovery journal is
modified.

The driver drains assigned IddCx swapchains on the exact DXGI render adapter;
Pier remains responsible for independently capturing the resulting Windows
outputs and encoding them. The feature is capability-gated and default-off in
`%ProgramData%\Arcen\pier.json`.

Validation without a WDK:

```text
test-driver-portable.cmd
./test-driver-portable.sh
verify-driver-source.ps1
```

Unsigned WDK validation:

```powershell
.\build-driver.ps1 -Configuration Release -Platform x64
```

The exact source manifest rejects missing, changed, or additional files and
forbidden driver/certificate/build payloads. The build script re-verifies that
manifest and forces `SignMode=Off`.

The operator gate is intentionally redundant:

```json
{
  "platform": {
    "iddcx": {
      "enabled": true,
      "render_adapter": {
        "stable_id": "host-specific-stable-adapter-id"
      }
    },
    "multi_monitor": {
      "advertise_enabled": true
    }
  }
}
```

Pier withholds the offer unless the exact ABI, complete capability bitmap,
adapter state, idle topology, exact adapter selector, and output enumeration
all verify. The LocalSystem broker opens the protected control device and
passes only that inherited handle to the session agent.

`arcen-iddcx.vcxproj` targets x64 UMDF 2.33 and IddCx 1.4 (Windows 10 1903+).
WDK 10.0.26100 on pier-windows.example.internal compiled and linked Release/x64 with no warnings;
INF signability and catalog generation also passed. No driver was installed.
An unsigned WDK build is development evidence only. Do not install, sign,
stage, or deploy this driver without Release/Security-approved certificate and
Microsoft signing inputs. Windows Server distribution remains blocked on an
approved WHQL/HLK path.
