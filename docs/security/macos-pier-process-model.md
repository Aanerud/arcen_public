# macOS Pier: why it runs as separate processes and a service account

The macOS Pier package installs more than one app bundle. It also creates a
hidden account called `_arcen`, and it raises macOS prompts that other apps do
not. The obvious question is why it is not one app. This page gives the reason
for each piece, what each piece can and cannot do, and what is still open.

The short answer: macOS requires two things of a remote-desktop host that no
single process can satisfy at once. The split also keeps the code that faces
the network away from everything it does not need.

Version note: three details below arrive in the package after 0.13.0. The
Agent Helper moves from `/Applications/Arcen Agent Helper.app` to
`/Library/PrivilegedHelperTools` (installing the newer package moves it). Both
launchd definitions name the Pier app. And declining the account prompt stops
the install with a reason. The design itself is the same in 0.13.0.

## The two requirements that force the split

**1. Capture and input work only inside a signed-in session, as that person.**
Screen capture, synthetic keyboard and pointer events, and the pasteboard all
belong to a graphical session. The privacy approvals that allow them (Screen &
System Audio Recording, Accessibility) are also held per person and per app.
A system service started at boot has no session, so macOS will not let it
capture or type. The part that does this must be a launch agent. launchd starts
one in each graphical session and stops it at logout.

**2. The host must stay reachable when nobody is signed in, and across user
switches.** There is one listening port (UDP 18444) and one TLS private key.
An agent dies with its session. With two people signed in, two agents would
compete for the same port, and every account's agent would need to read the
key. An earlier package did exactly that: it ran the whole host as a launch
agent. The next account to sign in could not read the key, so the host stopped
listening. See `hosts/macos/PACKAGING.md`, "Nothing belongs to the person who
ran the installer". The listener must therefore be a launch daemon that starts
at boot and belongs to the machine.

Each way of merging them breaks one of the two:

| Merged into | What breaks |
| --- | --- |
| The per-session agent | Nothing listens at the login window or after logout, users fight over the port, and every user can read the host key |
| The boot-time service | macOS refuses capture and input: there is no session |
| A root service that reaches into each session | The whole host, including code that parses untrusted network input, runs as root |

The Linux and Windows Piers make the same split for the same reason: a system
service that owns the network and a per-session agent that owns the desktop.

## What runs, as whom, and what it can do

| Process | Bundle | Started by | Runs as | Can | Cannot |
| --- | --- | --- | --- | --- | --- |
| Network service | `/Applications/Arcen Pier.app` (`pier.arcen.tech`) | launchd, at boot (`/Library/LaunchDaemons/pier.arcen.tech.service.plist`) | `_arcen` | Bind UDP 18444, read the host key, check passwords through PAM, relay an admitted Deck to an agent | See or control any desktop; it refuses to start as root |
| Desktop agent | `/Library/PrivilegedHelperTools/Arcen Agent Helper.app` (`pier.arcen.tech.agent`) | launchd, in each Aqua session and at the login window (`/Library/LaunchAgents/pier.arcen.tech.agent.plist`) | The signed-in person; **root at the login window** | Capture, encode, inject input and use the pasteboard, only as far as that person's privacy approvals allow | Bind the port or read the host key; serve an account other than its own |
| Virtual keyboard and pointer | The Pier's own executable, run as `arcen-pier-macos hid-injector` | The agent, as its child | The same account as the agent | Present one virtual HID keyboard and one absolute pointer, fed by the agent over standard input | Anything else: it has no network, no session and no policy |

Where each claim is enforced:

- **The service refuses root.** `hosts/macos/src/main.rs` (`run_daemon`) exits
  when started as root, unless a developer sets `ARCEN_DAEMON_ALLOW_ROOT`. The
  packaged definition never does. `hosts/macos/src/service.rs` renders `UserName _arcen`
  into the daemon definition, and the test
  `the_daemon_never_runs_as_root` checks it.
- **The agent has no key and no port.** `run_agent` in
  `hosts/macos/src/main.rs`. The TLS directory is `_arcen:admin 750` and the key
  is `600` (`hosts/macos/src/host_cert.rs`).
- **Each side verifies the other.** The service checks the connecting agent's
  uid from the kernel, and its executable path, against the installed helper
  (`hosts/macos/src/relay.rs`, `expected_agent_program`). The agent refuses a
  socket served by anyone other than root or the service account (`park` in
  the same file).
- **An agent serves only its own account.** `session_policy.serving_uid` in
  `run_agent`. A different account that authenticates is told that the screen
  belongs to someone else.
- **The virtual HID child is minimal.** `hosts/macos/src/input/virtual_keyboard.rs`.
  It exists because only the `pier.arcen.tech` signature carries
  `com.apple.developer.hid.virtual.device`, while the Agent Helper holds the
  Accessibility approval the kernel also requires. If the agent goes away, the
  child's input closes and it releases every key and button before exiting.

## Why a dedicated `_arcen` account

The service needs an identity that is not root and not a person. `_arcen` is
that identity, nothing more (`packaging/macos/pier/preinstall`):

- password `*`, so no password can log in to it;
- shell `/usr/bin/false`, home `/var/empty`;
- hidden, with an id below 500, so it never appears at the login window or in
  Users & Groups.

Nobody can sign in as `_arcen`. It exists so that a flaw in the network-facing
code yields an account that can read one key and bind one port, instead of the
whole machine. The alternatives are worse:

- **Root:** any single mistake in TLS, QUIC or message parsing becomes a total
  compromise. Rejected by this repository and by Apple's guidance to run only
  the smallest necessary piece with privilege.
- **`nobody`:** shared with other daemons, so it cannot own the host key
  exclusively.

`uninstall.sh --purge` deletes the account again.

## What the installer asks, and why

| Prompt | Cause | When |
| --- | --- | --- |
| Administrator password | Installing into `/Library` and `/Applications` | Every install |
| "Installer would like to administer your computer" | Creating the `_arcen` account. macOS guards changes to local accounts behind this approval (the TCC service `SystemPolicySysAdminFiles`). | First install on a Mac only; an upgrade finds the account and creates nothing |
| Screen & System Audio Recording, Accessibility for **Arcen Agent Helper** | The agent's capture and input | Once per person who will be served |
| System Audio Recording for **Arcen Agent Helper** | Core Audio process taps, which carry host audio and silence the host's speakers during a session. A separate approval from screen recording. The helper asks for all three approvals as soon as it starts, one at a time: system audio, then Accessibility, then Screen Recording. Each waits up to two minutes for the one before it, so the dialogs never stack. | Once per person who will be served. Until it is allowed, a host whose `audio.local_playback` is `muted` (the default) refuses sessions, and the Deck is told why |
| Screen capture at the login window | Serving the login window before anyone signs in | The first time a Deck connects there |
| "Background Items Added" notification | macOS reports new launchd jobs. Both definitions carry `AssociatedBundleIdentifiers = pier.arcen.tech`, so System Settings attributes them to Arcen Pier (`hosts/macos/src/service.rs`) | Once, after install |

The account prompt was measured on the lab Mac. The Installer raised
`SystemPolicySysAdminFiles` in the same second that `preinstall` logged
"creating service account _arcen". The same `dscl` call, run from a background
launchd job instead, was refused with `eDSPermissionError` and no prompt at
all. The prompt is therefore the only route. Declining it makes the install
fail, with the reason in the install log (`preinstall`), rather than leaving
launchd to fail without explanation.

The prompt does not come from `/Library/PrivilegedHelperTools`. Placing a
helper there triggers no approval of its own.

## Why not `SMAppService`

macOS 13 and later can register launchd jobs from inside an app bundle with
`SMAppService.daemon(plistName:)` and `SMAppService.agent(plistName:)`. That
would change where the definitions live, not the design above. It still needs
two processes and a non-root account. It also does not remove the account
prompt, because it creates no accounts. And it costs the host something that
matters:

- Only the app itself can register the job, so someone must open Arcen Pier
  after installing it. The package alone could not start the host.
- Apple documents registration as "subject to user approval". Until someone
  approves it in Login Items, or an MDM profile pre-approves it, the service
  does not run. A host that nobody is sitting at cannot wait for that.
- Whether an `SMAppService` agent may run at the login window has not been
  verified.

The package therefore installs ordinary launchd definitions, which start at
boot with no one at the Mac.

## Open: the login window agent runs as root

At the login window launchd starts the agent as root. That includes the capture
and encode pipeline and the code that handles Deck input. The service that
faces the network is still `_arcen`, and the agent is reached only through the
verified local socket. Even so, this is the one place where more code holds
privilege than needs it.

The intended fix follows the same principle as the rest of this page: keep
only the virtual HID device in a root process, and run capture and input
handling unprivileged. This has not been built. It must first be measured
whether capture at the login window still works after privileges are dropped.

## Related

- `hosts/macos/PACKAGING.md`: building, notarizing, installing, uninstalling,
  and the full list of files a package leaves.
- `docs/security/trust-boundaries.md`: the other trust boundaries in Arcen.
