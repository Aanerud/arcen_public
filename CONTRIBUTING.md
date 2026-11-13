# Contributing to Arcen

Thank you for thinking about it. Arcen Deck and Arcen Pier are free software,
and they get better when the people who use them every day — artists,
administrators, studios, hardware makers — send back what they learned.

Maybe your studio needs a feature that Arcen does not have yet. Maybe you make
a tablet, a control surface or a colour probe and want it to work properly
through a remote desktop. Maybe you fixed something on a Friday night and want
everyone else to have the fix. All of that is welcome here.

## What makes a good contribution

- **A feature or a device integration.** Open an issue first and describe what
  you want to build and why. A short design discussion up front saves you from
  building something that does not fit the architecture. For device work, say
  which hardware you can test on.
- **A bug fix.** Include how to reproduce the bug and how you proved the fix.
- **Documentation.** Corrections and clearer explanations are as valuable as
  code.
- **Measurements.** Latency, colour accuracy and pen behaviour are measured,
  not assumed. Reproducible numbers on real hardware are contributions too.

Security problems are the exception: please report them privately, as
[SECURITY.md](SECURITY.md) describes, not in a public issue or pull request.

## How to contribute

1. Fork https://github.com/Aanerud/arcen_public and create a branch. To build
   and test, start with [Building from source](docs/project.md#building-from-source):
   its prerequisites checklist and the per-platform build and test commands.
2. Make your change. Two things are asked of every change:
   - **Put logic in `shared/` if it can live there.** Code in `clients/` or
     `hosts/` should be code that genuinely cannot be written portably. The
     `AGENTS.md` file in each directory says who owns it and how to validate
     it; read the one for the area you are changing.
   - **Say what you actually built and ran.** Continuous integration does not
     build the platform crates, so your description is the evidence. "Built on
     Linux, ran the Pier tests, did not try Windows" is a good pull request
     description. Silence is not.
3. Give every new source file the project's licence header:

   ```text
   SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
   Copyright (C) 2026 TUTTIFRUTTI AS
   ```

   in the comment syntax of the file. Under the Contributor License Agreement
   you keep your own copyright, and you may add your own copyright line below
   these two. Files that cannot carry a comment are covered by `REUSE.toml`;
   `reuse lint` checks the result.
4. Open a pull request and fill in its template (what you built and tested,
   the CLA, and that it contains no private data). On your first one, accept
   the Contributor License Agreement (see below); after that, every pull
   request you open is covered.

## The Contributor License Agreement

Before a pull request can be merged, its author accepts the
[Contributor License Agreement](CLA.md) (CLA), with a statement in the pull
request ([How to accept](CLA.md#how-to-accept)). In plain language:

- **You keep your copyright.** The CLA is a licence, not an assignment. Your
  work remains yours, and you can use it however you like.
- **It lets TUTTIFRUTTI AS ship official builds.** TUTTIFRUTTI AS stewards
  Arcen and offers official signed and notarised builds through app stores,
  pre-installed cloud workstation images, and support. Some of those channels
  have terms that the AGPL alone cannot accommodate — app store terms, for
  example. The CLA gives TUTTIFRUTTI AS the licence it needs to include your
  contribution in those builds and images.
- **Your work stays free.** The CLA also commits TUTTIFRUTTI AS to keep every
  accepted contribution available to everyone under the AGPL-3.0 (with the
  [Store Distribution Exception](LICENSE-EXCEPTION.md)) in this repository. It
  can never be used to take your work away from the public.
- **It includes a patent licence**, as most open-source contributor agreements
  do, so that nobody who uses Arcen has to worry that a contribution comes with
  a patent claim attached.
- **You confirm the work is yours to give.** If your employer or client may own
  what you write, make sure they agree — or ask them to accept the entity
  agreement described below.

### Accepting the CLA as an individual

Read the [CLA](CLA.md), then paste the Part A statement from
[How to accept](CLA.md#how-to-accept), with your name (or handle) and GitHub
account filled in, into the description of your first pull request. A
maintainer records it and says so on the pull request. It covers all of your
future pull requests. A pull request is not merged until its author's
statement has been recorded.

### Contributing on behalf of a company or studio

If you contribute as part of your job, your employer usually owns the work. In
that case your organisation accepts Part B of the [CLA](CLA.md), the entity
agreement: a person authorised to bind it pastes the Part B statement from
[How to accept](CLA.md#how-to-accept) into a pull request, listing the people
who may contribute on its behalf. Each listed person then mentions the entity
agreement in their first pull request so the maintainers can match it.

## How credit works

Every contributor whose work is accepted is credited **by name or by handle —
your choice**:

- in [CONTRIBUTORS.md](CONTRIBUTORS.md), permanently; and
- in the release notes of the release that first contains your work.

Tell us in your pull request how you want to be credited. If you say nothing,
your GitHub handle is used. You can change it later by opening an issue or a
pull request.

## A community effort

Contributing to Arcen is voluntary. It is a community effort, not a paid
arrangement: neither you nor TUTTIFRUTTI AS owes the other payment for
contributions, and contributing does not create a support or service
relationship. If your organisation wants paid help — development, integration
or support — that is a separate conversation with TUTTIFRUTTI AS, outside this
repository.

## Forks

Forks and derivative work are welcome, whether or not they ever come back
upstream. If you distribute your own version, give it its own name and
branding, as [TRADEMARKS.md](TRADEMARKS.md) explains. See
[Community](docs/project.md#community) for how notable forks, ports and integrations
are recognised.
