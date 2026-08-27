# LEXICON: the canonical vocabulary of Arcen

Reviewed commit `eb6373e14addd45b537325d799fe02fcee39a4d1`.

This is the reference a future rename wave follows. It is descriptive where the
codebase is already consistent and prescriptive where it is not. Every "banned"
entry lists where the banned form actually appears, so the rename is mechanical.

The rename wave should land as one atomic change (README.md, Wave 2). Doing it
piecemeal is worse than not doing it, because a half-renamed vocabulary is harder
to search than a consistently wrong one.

## 1. Product nouns

| canonical term | definition | approved names in code | banned synonyms now in the code | where the banned form appears |
| --- | --- | --- | --- | --- |
| **Pier** | The host service that owns a physical workstation and serves its desktop. One per host OS. | crate `arcen-pier-linux`, `arcen-pier-windows`; binary `arcen-pier`; log target `arcen::session` | "broker" | `hosts/windows/src/resume.rs:66` ("broker-owned registry"), `hosts/windows/src/auth.rs:1` ("machine broker"), `hosts/windows/src/session.rs` `BrokerAgentLease`, `ResumeRegistryError::BrokerShutdown` at `hosts/windows/src/resume.rs:231` |
| **Deck** | The client application a user runs to reach a Pier. | crate `arcen-deck-macos`; binary `arcen-client`; app bundle `Arcen Deck.app` | "client" as a *product* noun (fine as a *role* noun) | binary name `arcen-client` in `clients/macos/src/main.rs:36-39` contradicts the product noun |
| **Span** | The relay/gateway tier. Roadmap only; no code in this repository. | none yet | "gateway", "relay" | n/a — if it is built, the crate and directory should be named `span`, not `gateway`. |
| **capenc** | The separate capture-and-encode child process both Piers spawn through their own multicall executable. | crate `arcen-capenc`; Pier subcommand `capenc` | none | consistent |
| **Keel** | The damage-tracking / dirty-block subsystem. | crate `arcen-keel` | none | consistent |
| **session** | One authenticated client attachment to one desktop. | `ActiveHostSessionId`, `session_log_id`, `sid` log field | "connection" when a session is meant | mostly consistent |
| **resume grant** | The HMAC-signed token that lets a Deck reattach without a credential. | `DirectResumeGrantToken`, `resume_grant` | "reconnect token" | consistent in code; prose in `docs/architecture/session-auto-reconnect.md` mixes "reconnect" and "resume" |
| **holder nonce** | The 32 random bytes the Deck generates to bind a grant to itself. | `DeckHolderNonce`, `resume_holder_nonce` | none | consistent |
| **admission** | The decision that permits one more concurrent session. A Pier drives one physical desktop, so this is a hardware constraint, not a commercial one. | `admit_new`, `SessionAdmissionLease` | "entitlement", "licence" | `hosts/linux/src/session_admission.rs`, `hosts/windows/src/session_admission.rs` |
| **deskside** | Operator-enforced physical-console privacy mode. | `deskside` | none | consistent |

## 2. Naming conventions

### 2.1 Modules

Observed: `snake_case`, one concept per file, mostly good. Two competing layouts,
see PARITY.md D2.

Recommended: directory modules once a concern exceeds about 800 lines. Adopt the
Linux layout on both hosts. A `mod.rs` re-exports; it does not hold logic.
`hosts/linux/src/logging/mod.rs` at 976 lines violates its own convention and
should be split.

### 2.2 Traits

Observed, and inconsistent:

| form | examples |
| --- | --- |
| capability, `-able`/verb-phrase | none |
| role noun | `KeyRingSource` (Linux), `KeyProvider` (Windows), `FactProvider`, `LicenseClock`, `SecureStore`, `ResolvesServerCert` (rustls) |
| action noun | `DirectResumeGrantSigner`, `DirectResumeGrantVerifier` |

Recommended: role nouns ending in `Source`, `Provider`, `Store`, `Clock`, or the
agent form (`Signer`, `Verifier`). Pick one per concept and never two: today the
same concept is `KeyRingSource` on Linux and `KeyProvider` on Windows (CONS-001).

Canonical: `VerificationKeySource`, `HostFactSource`, `LicenseClock`,
`SecureStore`, `DirectResumeGrantSigner`, `DirectResumeGrantVerifier`,
`ResumeHostAdapter`.

### 2.3 Structs

Observed: `UpperCamelCase`, descriptive, generally good. Newtypes are used well
in `shared/identity` (`HostIdentity`, `ActiveHostSessionId`, `WindowsSid`,
`DeckHolderNonce`, `DirectResumeNonce`).`SignatureHex`, `HostId`).

Recommended: keep this. Extend the same discipline to `shared/input`, which still
passes bare primitives (API-305).

Redaction convention, observed and good: types holding secrets implement a manual
`Debug` that prints `<redacted>` or a length. Examples:
`hosts/linux/src/session/resume.rs:118`,
`hosts/windows/cp-ipc/src/crypto.rs:194-202`,
`shared/media/src/clipboard/mod.rs:325`. Make this a written rule: any type whose
field names include key, secret, token, credential, password, nonce, or grant
must have a manual `Debug`.

### 2.4 Enums and variants

Observed: `UpperCamelCase` variants, generally good. Error enums carry a reason
per variant rather than a string, except on the Windows auth path.

Recommended: variant names state the *condition*, not the *reaction*.
`ProductionKeyNotConfigured` is right. `Crypto` (at
`hosts/linux/src/session/resume.rs`) is too vague: prefer `SignatureInvalid` or
`KeyDerivationFailed`.

### 2.5 Error types

Observed: 60+ distinct error types. Suffix `Error` is universal, which is good.
Shape is not:

| shape | examples |
| --- | --- |
| typed enum, no payload strings | `LicenseError`, `ProtocolError`, `CryptoError`, `DirectResumeError`, `AuthError` |
| typed enum with `&'static str` payload | `AuthError::InvalidRequest(&'static str)`, `CryptoError::BadParameter(&'static str)` |
| bare `String` | `hosts/windows/src/auth.rs:106`, `hosts/windows/src/session.rs:2337-2352`, `hosts/windows/src/auth.rs:317-325` |

Recommended taxonomy, applied everywhere:

```rust
pub enum XxxError {
    /// The peer sent something invalid. Not our fault, do not retry.
    Invalid(&'static str),
    /// A transient condition. Retry is meaningful.
    Unavailable(&'static str),
    /// A policy said no. User-actionable.
    Denied { reason_class: &'static str, recovery_action: &'static str },
    /// We broke. Log with context, report as internal.
    Internal(&'static str),
}
```

Use typed error classifications rather than inventing stringly APIs. Ban
`Result<_, String>` on any path that crosses a module boundary; it destroys
matching, classification, and telemetry.

### 2.6 Constructors and conversions

Observed and mostly consistent:

| form | meaning | examples |
| --- | --- | --- |
| `new(...)` | infallible construction from already-valid parts | `VerificationKey::new`, `DeckHolderNonce::new` |
| `new(...) -> Result<Self, E>` | construction that validates | `KeyId::new`, `WindowsSid::new`, `VerificationKeyRing::new` |
| `parse(...)` | construction from a string form | `DirectResumeGrantToken::parse`, `DisclaimerVersion::parse_lower_hex` |
| `from_parts(...)` | reassembly from wire fields | `SealedCredential::from_parts` |
| `with_*(...)` | builder-style, consuming, chainable | `AuthResponse::with_displays`, `AuthRequest::with_resume_support` |
| `try_from` | `TryFrom` impls for wire enums | `FrameType`, `VideoCodec`, `ChromaSubsampling` |

Recommended: keep exactly this, and make one rule explicit that is currently
implicit: a fallible `new` is acceptable, but if construction can fail for more
than one reason the type should say so with a typed error, not `Option`.

### 2.7 Async functions

Observed: no naming distinction between blocking and async. `authenticate_windows`
is async and `authenticate_windows_blocking` is its blocking inner function
(`hosts/windows/src/auth.rs:102` and `:138`), which is the right pattern and is
used in only that one place.

Recommended: adopt it everywhere. A blocking function called from an async
context via `spawn_blocking` carries the `_blocking` suffix. A function that must
not be called from an async context and has no async wrapper says so in its doc
comment under `# Blocking`.

### 2.8 Config keys

Observed: `snake_case`, dotted paths, two sections: a common section identical on
both platforms (genuinely good) and a `platform` section that has diverged
(PARITY.md D16 to D23).

Recommended rules:

1. `platform.*` holds only settings with no meaning on the other OS.
   `pam_service`, `xorg_bin`, `first_login_timeout_secs` qualify. Display
   selection does not (CONS-002).
2. A key name states the concept, not the backend. `output_index`, not `monitor`.
3. Units are in the name: `_secs`, `_ms`, `_bytes`, `_days`, `_mb`,
   `_basis_points`. Already followed consistently, keep it.
4. Indices are 0-based and named `_index`; ordinals are 1-based and named
   `_number`. Never mix (CONS-002).
5. A boolean key names the enabled state: `enabled`, not `disabled`.

Banned key names and where they appear:

| banned | canonical | appears in |
| --- | --- | --- |
| `platform.capture.monitor` | `capture.output_index` | `packaging/linux/arcen-pier.json`, `hosts/linux/src/cli.rs:802` |
| `platform.desktop.output_index` | `capture.output_index` | `packaging/windows/pier.json`, `hosts/windows/src/config.rs:33` |
| `platform.desktop.deskside.*` | `platform.deskside.*` | `packaging/windows/pier.json` |
| `platform.desktop.deskside.monitors` | `platform.deskside.outputs` | `packaging/windows/pier.json` |
| `platform.auth.unsafe_allow_remote_no_auth` | delete entirely | `hosts/linux/src/config.rs:27`, `packaging/linux/arcen-pier.json` (SEC-001) |

### 2.9 Environment variables

Observed: 47 distinct `ARCEN_*` variables. Prefix is universal and correct. No
other convention holds.

| group | examples | judgement |
| --- | --- | --- |
| binary discovery | `ARCEN_CAPENC`, `ARCEN_AUDIOCAP`, `ARCEN_SESSION_AGENT`, `ARCEN_SESSION_LAUNCHER` | consistent, keep |
| runtime paths | `ARCEN_CONFIG_DIR`, `ARCEN_LOG_DIR` | consistent, keep |
| logging | `ARCEN_LOG` | keep |
| telemetry injection | `ARCEN_EVENT`, `ARCEN_EVENT_ID`, `ARCEN_EVENT_NAME`, `ARCEN_CATEGORY`, `ARCEN_SEVERITY`, `ARCEN_OUTCOME`, `ARCEN_CORRELATION_ID`, `ARCEN_SESSION_LOG_ID`, `ARCEN_FIELD_*` | 9+ variables where one structured value would do |
| security gates | `ARCEN_ACCEPT_INSECURE` | correct pattern: an explicit acknowledgement, not a silent default |
| **test-only, shipped in product binaries** | `ARCEN_LIVE_DISPLAY_TEST`, `ARCEN_LIVE_PIPELINE_TEST`, `ARCEN_LIVE_WATCHDOG_TEST`, `ARCEN_LIVE_DISPLAY_*` (6 more), `ARCEN_LICENSE_LOCK_TEST_MODE`, `ARCEN_LICENSE_LOCK_TEST_ROOT`, `ARCEN_ISSUER_TEST_KEYGEN_BARRIER`, `ARCEN_MEDIA_SMOKE_*`, `ARCEN_MEDIA_DUMP*`, `ARCEN_REQUIRE_WINDOWS_LICENSING_NATIVE_TESTS` | **17 test-shaped variables.** Any that a release binary still reads is a runtime behaviour switch reachable by anyone who can set an environment variable on the service. |

Recommended:

1. Naming: `ARCEN_<AREA>_<THING>`. `ARCEN_DISPLAY_RECOVERY_JOURNAL` follows it;
   `ARCEN_PDEATH_HELPER` does not.
2. Any variable whose name contains `TEST`, `LIVE`, `SMOKE`, or `DUMP` must be
   compiled out of release builds with `#[cfg(any(test, feature = "..."))]`, or
   renamed if it is genuinely a supported operator control. Which of the 17 are
   still readable in a release Pier is an open question this review could not
   settle; see README.md open questions.
3. Security gates keep the double-gate pattern: `ARCEN_ACCEPT_INSECURE` must be
   set *and* the Deck's security mode explicitly lowered, so neither alone
   disables certificate checking (see SEC-207).

### 2.10 Log targets

Canonical source: `shared/telemetry/src/names.rs`, which defines eight:
`arcen::auth`, `arcen::display`, `arcen::hid`, `arcen::health`, `arcen::media`,
`arcen::net`, `arcen::session`, `arcen::telemetry`.

It is not actually the source of truth. Every component re-declares its own:

| component | file | declares |
| --- | --- | --- |
| shared | `shared/telemetry/src/names.rs:6-20` | AUTH, DISPLAY, HID, HEALTH, MEDIA, NET, SESSION, TELEMETRY |
| linux | `hosts/linux/src/logging/mod.rs:32-42` | NET, TLS, AUTH, SESSION, MEDIA, CAPENC, DISPLAY, INPUT, AUDIO, HEALTH, LICENSE |
| windows | `hosts/windows/src/logging.rs:33-38` | CAPENC, CPPIPE, EVENTLOG |
| macos | `clients/macos/src/logging/mod.rs:29-30` | UI, INPUT |

Recommended canonical set, all defined in `shared/telemetry/src/names.rs` and
nowhere else:

```
arcen::audio     arcen::auth      arcen::capenc    arcen::clipboard
arcen::display   arcen::eventlog  arcen::health    arcen::hid
arcen::input     arcen::license   arcen::media     arcen::net
arcen::session   arcen::telemetry arcen::tls       arcen::ui
```

Banned, and where:

| banned use | canonical | appears in |
| --- | --- | --- |
| `arcen::hid` for injected input | `arcen::input` | Windows input path (CONS-166) |
| `arcen::media` for audio | `arcen::audio` | Windows audio path (CONS-166) |
| `arcen::cppipe` | `arcen::ipc` | `hosts/windows/src/logging.rs:37` |
| `arcen::test` | delete, test-only | 9 occurrences |
| `arcen::dependency` | fold into `arcen::telemetry` | 1 occurrence |

### 2.11 Telemetry field names

Canonical source: `shared/telemetry/src/schema.rs` and `names.rs`. Drift found:
`window_ticks` and `client_mtu` are emitted by `hosts/linux` and defined in
neither (CONS-306). Windows and macOS have no field drift.

Recommended: generate the field constants from the schema so an undefined field
name fails to compile (CONS-306).

### 2.12 Metric names

Not applicable at this commit. There is no metrics export surface, only
structured logs and `QosTargets` thresholds. When one is added, the naming rule
should be `arcen_<area>_<thing>_<unit>` and it should be defined in the same
place as the log targets.

### 2.13 Feature flags

Observed:

| crate | features |
| --- | --- |
| `shared/media` | `audio-opus`, `software-h264-source` |
| `shared/transport` | `quic` |
| `hosts/capenc` | `nvenc`, `mf`, `software-h264` |

Problems:

1. `hosts/capenc/software-h264` and `shared/media/software-h264-source` are two
   names for one capability. Pick `software-h264`.
2. `hosts/capenc/mf` is an unexplained abbreviation. Use `media-foundation`.
Recommended rules: features are additive capabilities, lowercase-kebab, named for
what they enable rather than how. A feature that weakens security must have an
explicit build-time chokepoint.

## 3. Prose vocabulary for documents and log messages

| use | do not use |
| --- | --- |
| Pier | broker, server, host service |
| Deck | client app, viewer |
| session | connection, attachment |
| resume | reconnect (reserve "reconnect" for the transport-level retry in `shared/session/src/direct_reconnect.rs`) |
| admission | entitlement check, seat check |
| grant | ticket, token (reserve "token" for the auth method that does not exist, see API-001) |
| output / display | monitor, screen (except in `ClientMonitor`, which is the Deck's own enumeration and is wire-visible) |
