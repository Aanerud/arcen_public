# macOS Pier: building, notarizing and installing the package

How to produce a `.pkg` another Mac will accept, and the install-time traps
that cost a full afternoon of "it installed fine and the client times out".

Identity and entitlements live in [`SIGNING.md`](SIGNING.md); this file is
about getting a working host onto a machine that has never seen one.

## Two certificates, not one

This is the first thing that surprises people. Signing an installer needs a
**different certificate** from signing the applications inside it:

| Certificate | Signs |
| --- | --- |
| `Developer ID Application: … (NWR7ZH8L7U)` | the `.app` bundles |
| `Developer ID Installer: … (NWR7ZH8L7U)` | the `.pkg` itself |

Having the first and not the second produces a package that builds, reports
success, and is refused by Gatekeeper on every Mac except the one that built
it. Check what is actually in the keychain before concluding anything is
missing:

```sh
security find-identity -v | grep -i "developer id"
```

## The build

```sh
cargo build --locked --release -p arcen-pier-macos

packaging/macos/build-pier-pkg.sh \
  --identity           "Developer ID Application: Tuttifrutti AS (NWR7ZH8L7U)" \
  --installer-identity "Developer ID Installer: Tuttifrutti AS (NWR7ZH8L7U)"
```

Both flags are optional and the script says so loudly when they are omitted —
it emits `ArcenPier-<version>.pkg.unsigned` and warns. That is fine for a
local test on the build machine and useless for anything else.

Build the release binary first. The app script assembles from
`target/release`, and a stale binary there is silently packaged as though it
were current.

## Notarize, staple, verify

Notarization is a separate step from signing, and the ticket has to be
stapled or the package only validates on a machine that can reach Apple.

```sh
xcrun notarytool submit dist/macos/ArcenPier-<version>.pkg \
  --keychain-profile "$NOTARY_PROFILE" --wait

xcrun stapler staple dist/macos/ArcenPier-<version>.pkg
```

Then confirm what a *clean* Mac will decide, rather than assuming:

```sh
spctl -a -vvv -t install dist/macos/ArcenPier-<version>.pkg
```

The verdict you want:

```text
accepted
source=Notarized Developer ID
origin=Developer ID Installer: Tuttifrutti AS (NWR7ZH8L7U)
```

`source=Notarized Developer ID` is the part that matters. A merely signed
package reports something else and will be refused.

`$NOTARY_PROFILE` names a keychain profile holding the Apple ID credentials,
created once with `xcrun notarytool store-credentials`. Confirm it exists
with `xcrun notarytool history --keychain-profile "$NOTARY_PROFILE"` rather
than discovering it is absent halfway through a release. The profile name is
local to whoever builds releases and is deliberately not recorded here.

## Verify the payload, not the script

A build script that ran without error is not a package containing what you
intended. Twice now the shipped `postinstall` has been missing something the
source clearly had. Unpack the real artefact:

```sh
cd "$(mktemp -d)"
xar -xf /path/to/ArcenPier-0.12.0.pkg
mkdir sx && cd sx
cat ../Scripts | gunzip -dc | cpio -i
grep -c 'launchctl enable' postinstall
```

This takes fifteen seconds and has caught real omissions.

## The three install-time traps

Each of these produced the same symptom — an install that reported success,
showed the app in `/Applications`, listed the helper in Login Items, held its
privacy grants, and never once listened on UDP 18444. The operator connects
and gets a timeout with nothing to read.

### 1. The host needs a configuration, and the installer must write it

Linux ships `packaging/linux/arcen-pier.json` to `/etc/arcen/pier.json`.
macOS ships `packaging/macos/arcen-pier.json` to
`/Library/Application Support/Arcen/pier.json`. Same shared schema — a Pier
is one product on three platforms — with only the platform section differing.

Written only when absent, so an upgrade never replaces an edited
configuration. The template is substituted into the `postinstall` at build
time from that one file, so there is no second copy to drift, and the build
fails if the marker is lost.

Without the file the host falls back to its built-in defaults, which are
deliberately fail-safe and copied from Linux's rather than invented:
**loopback only, audio off, microphone off**. A host nobody configured must
not expose itself to the network or redirect sound. The shipped template is
what makes a deliberate installation reachable — so the fall-back and the
template must differ in exactly that way, and a test asserts it.

Absent, named and malformed are three different things, settled by
`hosts/linux/src/config.rs:110-121` and matched here:

| Case | Behaviour |
| --- | --- |
| No file at the default path | Built-in defaults |
| No file at a path given with `--config` | Error — a typo must not silently yield defaults |
| File present, does not parse | Error |

### 2. `launchctl disable` is permanent, and poisons every later install

`launchctl disable gui/<uid>/<label>` writes an override into launchd's own
database. It outlives the job, the plist, the uninstall **and the next
install**. Every subsequent bootstrap of that label is then refused with:

```text
Bootstrap failed: 5: Input/output error
```

which names neither the label nor the reason, and looks exactly like a
malformed plist. Check for it:

```sh
sudo launchctl print-disabled gui/$(stat -f%u /dev/console) | grep arcen
```

Use `bootout` to stop an agent. Use `disable` only if you intend a machine to
keep refusing that label after a reinstall, which is essentially never — and
never in a cleanup script.

The installer runs `launchctl enable` before `bootstrap` for this reason: it
cannot know what a previous removal, an administrator or an older version
left behind, and an override it did not create is exactly what it must clear.

### 3. Never swallow the command that starts the product

This was the load step for a long time:

```sh
launchctl bootstrap "gui/$CONSOLE_UID" ... 2>/dev/null || true
```

Both the error text and the exit status discarded, on the single command that
decides whether anything runs. The same mistake is documented a few lines
above it in the same script, for the certificate step, and was still present
below.

Report it, and give the operator the two commands that recover. Treat
`already bootstrapped` as the success it is — an upgrade over a running agent
— and `kickstart` instead.

`launchctl enable`, `kickstart` and the permission request may keep their
suppression: the first is harmless when nothing was disabled, and the other
two are best-effort by design.

### 4. Nothing belongs to the person who ran the installer

An earlier package ran the whole host as a LaunchAgent in the console user's
session and handed `/Library/Application Support/Arcen` — key included — to
whoever was at the console during install. The next account to log in, an AD
user, could not read the key: `EX_CONFIG`, thirty-two restarts, no listener.
Its `/tmp` log then belonged to that user, which locked the first one out in
turn. And the install-time user could copy the identity every Deck had
pinned.

The package now installs two processes, the split the Linux and Windows
Piers already had: a `_arcen` LaunchDaemon that owns UDP 18444, the key and
admission from boot, and a LaunchAgent in every Aqua session that captures,
injects and holds the privacy grants, with no key and no port. The service
hands each admitted Deck to the agent of the session on the console over
`/Library/Application Support/Arcen/run/agent.sock`, checking the agent's uid
from the kernel and its executable path, and relays the stream. The agent
serves only its own account: a different account that authenticates is told
the screen belongs to someone else before it is told it succeeded.

Why this cannot be one process, why the service account exists, and what each
installer prompt is for are written up for administrators and users in
[`docs/security/macos-pier-process-model.md`](../../docs/security/macos-pier-process-model.md).
The installer's welcome page (`packaging/macos/pier/welcome.html`) gives the
same explanation in short. Keep the two in step when the design changes.

### The installer checks its own result

Because two different causes produced one symptom, the `postinstall` now
waits for the service to be running *and* bound to UDP 18444, and for an
agent in every logged-in session, and names whichever is missing. Package
scripts run with a minimal `PATH`, so it calls `/usr/sbin/lsof` by full path:
the first version of this check reported a healthy service as not listening
because `lsof` was not found.

An installer that verifies its own work converts a silent failure discovered
by a user into a message at install time.

## Installing: on the Mac, with Installer.app

The package is opened in Finder, on the Mac it installs, by the person who will
approve its privacy permissions. Its final page lists them. The package's
installation check (`packaging/macos/pier/distribution.xml`) refuses the
command-line `installer` before anything is installed:

```
$ sudo installer -pkg ArcenPier-0.12.0.pkg -target /
installer: Error - Arcen Pier must be installed with Installer.app on this Mac, not from Terminal. …
```

A Terminal or SSH install finishes with a host that accepts a Deck but shows no
picture and takes no keys. Nobody at the Mac has approved Screen Recording or
Accessibility, and nothing will tell them to. Administrators whose Macs get the
permissions some other way can opt in explicitly:

```sh
sudo ARCEN_ALLOW_COMMAND_LINE_INSTALL=1 installer -pkg ArcenPier-0.12.0.pkg -target /
```

This was measured on the lab Mac:
- The command-line installer sets `COMMAND_LINE_INSTALL=1`.
- The installation check sees the caller's environment, including the opt-in.
- The package scripts see neither the opt-in nor any other caller variable, so
  the check cannot live in `preinstall`.
- A refused check prints its message and exits 1.

## Privacy permissions: what the installer cannot do

The package sets up everything a root installer is allowed to:
- the `_arcen` service account;
- the host identity;
- configuration, logs and the PAM service;
- the firewall exception;
- the service, and an agent for each session that is logged in.

It cannot grant privacy permissions. macOS keeps those in TCC, which SIP
protects, and only a person or an MDM profile can change them. The installer
prints what is still needed:

| Permission | For | Who grants it |
| --- | --- | --- |
| Screen & System Audio Recording, "Arcen Agent Helper" | capture and system audio | each user, once, in their own session |
| Accessibility, "Arcen Agent Helper" | keyboard input, and virtual HID devices | each user, once |
| Screen capture at the login window | serving the login window before anyone signs in | someone at the Mac, the first time a Deck connects there |

On a managed Mac, a Privacy Preferences Policy Control (PPPC) profile can
pre-approve Accessibility for the Agent Helper's bundle ID and code
requirement. It cannot pre-approve Screen Recording: Apple allows only
`AllowStandardUserToSetSystemService`, which lets a standard user approve it
without an administrator. That consent always involves a person.

## Uninstalling, and verifying it

Check **paths**, never counts. `ls dir_a dir_b | grep -ci arcen` returning
`0` also happens when a `rm` failed behind `2>/dev/null`, when `ls` errored
on one directory, or when a shell glob aborted because one pattern had no
match. That combination once reported a clean machine with the launch agent
still installed, and somebody installed on top of it.

The uninstaller does this itself and fails if anything is left:

```sh
sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh"          # keeps identity, config, logs, _arcen
sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh" --purge  # removes those too
```

It boots the agent out of **every** graphical session (found through each
session's `loginwindow`), not only the console's, then the service, then
checks by path, by process and by UDP 18444.

Everything a full install leaves:

```text
/Applications/Arcen Pier.app
/Library/PrivilegedHelperTools/Arcen Agent Helper.app
/Library/LaunchDaemons/pier.arcen.tech.service.plist   (network service, runs as _arcen)
/Library/LaunchAgents/pier.arcen.tech.agent.plist      (desktop agent, every Aqua session)
/Library/Application Support/Arcen/   root:wheel 755   (pier.json root 644)
/Library/Application Support/Arcen/tls/   _arcen:admin 750, host.key 600
/Library/Application Support/Arcen/run/   _arcen, the agent socket
/Library/Logs/Arcen/Pier/   _arcen:admin 750   (service.log, service jsonl)
~/Library/Logs/Arcen/Pier/  each user's agent.log and agent jsonl
/etc/pam.d/arcen
/etc/newsyslog.d/pier.arcen.tech.conf
the _arcen user and group (dscl, id below 500)
pkgutil receipt: pier.arcen.tech
```

Earlier builds also left `/Library/LaunchAgents/pier.arcen.tech.plist` and
`/tmp/arcen-pier.{out,err}.log`; the installer and the uninstaller remove both.

`/etc/pam.d/arcen` is the one a hand-written list always misses — it is the
only artefact not named `pier.arcen.tech` and not under a directory anyone
thinks of as Arcen's. So finish with a sweep:

```sh
find /Applications /Library /etc/pam.d /etc/newsyslog.d "$HOME/Library" -maxdepth 4 -iname '*arcen*'
sudo launchctl print-disabled system | grep arcen
launchctl list | grep arcen
pkgutil --pkgs | grep arcen
lsof -iUDP:18444
```

Two things an uninstall cannot reach:

- **launchd keeps a job registered in memory** after its plist is deleted, so
  it has to be booted out by label separately.
- **TCC grants survive everything.** SIP makes that database read-only and the
  rows are keyed to the bundle ID, so a reinstall inherits the permissions
  rather than re-prompting. Testing a genuine first-run experience means
  clearing them by hand in System Settings first.

## The lab machine is a delivery target

Mac-S-08 exists to answer one question: does the shipped package install, run
and uninstall on a machine that never built it. That is only worth anything
while it stays a machine that never built it.

Nothing developer-side runs there — no `xcrun`, no `cargo`, no `clang`, no
toolchain install, no compiling a scratch probe in `/tmp`. Invoking `xcrun`
alone raises the Command Line Tools installer on its console, which both
interrupts whoever is sitting there and quietly makes the clean machine
dirty. Build here, copy the artefact across, run it there.
