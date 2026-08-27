# Session privacy: nobody but the session owner sees the screen

**Status:** outcome statement, not a design. Written 2026-07-28 for a follow-up agent.
**Owners:** Windows Host, Linux Host, with Release/Security on the trust boundary.

## The outcome we want

While a remote user is working through Arcen, **no one else may see that screen and
no one else may drive it** — whatever the screen happens to be attached to.

Two situations, both real:

1. A physical workstation in an office. Somebody walks past, or sits down at the
   attached monitors.
2. A VM. Somebody opens the hypervisor or VDI console — IT support, a platform
   admin, anyone with console rights to the guest.

In both, the onlooker is not the session owner and should see a black screen, and
their keyboard and mouse should do nothing to the session.

## What already exists

**Deskside** covers situation 1 and is implementation-complete:
`hosts/windows/src/deskside.rs`, `hosts/linux/src/deskside.rs`, and
`shared/session/src/deskside.rs`. It pins every physical monitor by hash, blanks
them for the session, and takes ownership of local input with low-level hooks that
swallow physical keyboard and mouse while passing Arcen's own injected events.

It is **disabled by default and not released**. Per
`docs/operations/deskside-recovery.md`, the Windows and Linux physical validation
matrices are unrun mandatory gates; VM results only prove refusal and
orchestration.

## What does not exist

**Situation 2 is deliberately refused, not solved.**
`shared/session/src/deskside.rs:169-188` lists `VirtualEvidence`,
`ParavirtualEvidence` and `RemoteEvidence` as refusal reasons, and
`DesksidePolicy::decide` is binary — `Arm` or `Refuse`. A VM therefore cannot arm
deskside at all.

That refusal is honest rather than lazy. `hosts/windows/ARCHITECTURE.md:382-394`
requires CPUID no-hypervisor plus pinned SMBIOS chassis facts, because on a
physical box the guest can prove it blanked every attached panel. Inside a VM it
cannot: the hypervisor owns the framebuffer, and a guest that blanked its own
outputs would still be mirrored by the console. Claiming privacy we cannot deliver
would be worse than declining it.

So the gap is not "make deskside work on VMs". It is: **what can a guest honestly
promise about console privacy, and what must the platform provide?**

## What a follow-up should decide

1. **Is a guest-only promise ever sound?** If the hypervisor can always mirror the
   framebuffer, no amount of guest code fixes it, and the answer is a platform
   control (vSphere/Hyper-V/Proxmox console permissions, or a vGPU head that has no
   console at all) plus documentation — not a code change.
2. **If it is partially sound**, what is the honest boundary, and how is it stated
   to an operator so nobody believes they have protection they do not?
3. **Does the binary `Arm`/`Refuse` policy need a third state** for "protected
   against local input, not against console observation"? A partial guarantee must
   be impossible to mistake for a full one.
4. **Release the physical case first.** Deskside for situation 1 is built and
   blocked only on the physical validation matrices. Shipping that is worth more
   than a speculative VM story.

## Constraints for whoever takes this

- Deskside is security-critical and fail-closed. Weakening a refusal to widen
  coverage is the wrong trade; refusing loudly beats protecting partially and
  silently.
- Do not weaken the recovery journal ACLs, follow reparse points, or delete a
  failed journal — see `docs/operations/deskside-recovery.md`.
- Any change to the trust boundary needs Release/Security review.
- The physical validation gates are mandatory and unrun. They gate release
  regardless of what is decided about VMs.

## Not this

Issue #104 (Deck renders a blank window after a prior session) is **unrelated**.
That is the macOS client failing to present its own UI — the saved-connections
list, before any session exists, reproducible against a `main` build with every
thread idle. It is a client rendering fault, not privacy blanking.
