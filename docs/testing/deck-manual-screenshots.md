# Deck manual screenshots

The user manual on the website shows every Deck scene in light and dark, for
the macOS Deck and the Windows Deck, with numbered hotspots on the controls
it explains. The pictures come from the Deck itself: `arcen-deck doc-shots`
draws each scene with the real shared screens, inside the shell's own egui
setup and root frame, renders it offscreen with egui-wgpu (Metal on a Mac,
DirectX 12 on Windows) and writes PNGs. It needs no visible window, no one
at the desk and no screen recording, so it also runs with the screen locked
or over a remote shell.

Beside the PNGs it writes `deck-shots-geometry.json`: for every scene,
platform and theme, the centre of each hotspot's target as a percentage of
the image, from the rectangle egui reports for that widget.

## What is in a scene

The scene list is the website's `deck-shots/manifest.json` (17 scenes), then
four scenes the manifest does not list yet: `home-own-settings`,
`sign-in-refused`, `settings-privacy` and `connecting-one-display` (a run
says each "is not in the manifest" and goes on).
Each scene is built in `shared/deck-core/src/doc_shots/scenes.rs` from the
real shared types, and its outcomes come from the real shared mappers:

| Scene | Built from |
| --- | --- |
| Home, menus, forms, picker | saved connections, `apply_home_action`, clicks on the real widgets |
| Connection test | `ConnectionTest::start` with `connection_diagnostics::assess` of a name that resolves and a Pier that does not answer |
| Sign-in error | Connect, then the engine's session end for a refused password, `SignInFailure::after_end`, `end_detail`, `SignInState::return_home` |
| Home's own-settings line | a saved connection with its own Typing and Displays, selected; the summary under Connect and **Use app defaults** |
| Sign-in refused | Connect, then the engine's session end for a Pier whose desk has nobody signed in (`ConsoleRefusal::NoConsoleSession`); its headline in the acknowledgement box |
| Connecting on one display | **Use Primary Display Only** for Grade 1: `displays_unsupported_retry`, the attempt's `SessionDisplaysOrigin`, `connecting_status`, `render_connecting` |
| Verify this host, identity changed | `CertInfo` with fingerprints from a fixed seed, `identity_change_outcome` |
| Import review | an exported `.arcen` file read back with `ConnectionProfile::parse` and reviewed |
| Settings pages | `SettingsValues` from the default settings, `render_settings` |

Only placeholders appear: `grade-1.example.internal`,
`edit-2.example.internal`, `203.0.113.10` (and `203.0.113.11` for the
address Grade 1's name stands for), the user `alice`, folders under
`/Users/alice` or `C:\Users\alice`. Nothing touches a network, the user's
settings or the keychain: Home never looks the names up, the settings
repository lives in memory, and host presence stays unknown (`NoPresence`).

Light and dark are what the harness tells egui the window system reports;
the operating system's appearance is never changed. Animation is instant
(Reduce motion), and the pointer leaves the window after each click, so no
hover or tooltip is in the picture.

## Running it

The feature is developer-only. A Deck built with it carries the do-not-ship
marker, so `packaging/macos/build-deck-app.sh --release` and
`packaging/windows-deck/build.ps1` refuse it. It does not enable `dev-tools`,
which would add developer overrides to Settings.

macOS Deck, on a Mac:

```sh
cargo build --locked --release -p arcen-deck-macos --features doc-shots --bin arcen-deck
target/release/arcen-deck doc-shots --out deck-shots/mac \
  --manifest ../arcen_tech/deck-shots/manifest.json
```

Windows Deck, on Windows (PowerShell):

```powershell
cargo build --locked --release -p arcen-deck-windows --features doc-shots --bin arcen-deck
target\release\arcen-deck.exe doc-shots --out deck-shots\windows `
  --manifest ..\arcen_tech\deck-shots\manifest.json
```

Options:

| Option | Does |
| --- | --- |
| `--out DIR` | where the PNGs and `deck-shots-geometry.json` go (required) |
| `--scene ID` | only this scene; repeat for more |
| `--theme light\|dark` | only this theme |
| `--manifest FILE` | check that every scene and hotspot label in the manifest is mapped here |
| `--widgets` | also write every widget egui reported, per image, to `DIR/widgets/` |

Files are named `deck-{scene}-{platform}-{theme}.png`, 2240 × 1520 pixels
(the 1120 × 760-point window at 2×). A second run into the same folder for
the other Deck adds its entries to the same geometry file. The command exits
1 if a mapped hotspot's widget is missing or the manifest disagrees, 2 for a
usage error.

## Hotspots

`shared/deck-core/src/doc_shots/hotspots.rs` maps each manifest label to one
widget by its accessible name: its text, the name an icon button gives
itself, a prefix of a long text, or the control under a caption. A hotspot
that names a group ("Username and password", "Cancel and Add connection")
or something with no widget on screen (a choice inside a closed picker) is
written as `unmapped`, with the parts it spans where they exist; a target
outside the image is marked `"in_image": false`. Pick those positions by
hand on the website.

When a screen changes, run with `--widgets`, find the new name in
`DIR/widgets/*.json`, and update the table. The unit test
`doc_shots::tests` (feature `doc-shots`, no GPU) checks that every scene
lays out in both themes on both Decks and that every mapped hotspot finds
its widget:

```sh
cargo test --locked -p arcen-deck-core --features doc-shots --lib doc_shots
```
