# DOC-STANDARD: the house format for ARCHITECTURE.md and AGENTS.md

Every folder that owns code carries two documents. They answer different
questions for different readers and must not duplicate each other.

- `ARCHITECTURE.md` answers "what is this and how does it work" for a human
  engineer reading the area cold.
- `AGENTS.md` answers "what may I change here and how do I prove I did not break
  it" for an AI agent or a new contributor about to edit the area.

Both are checked against the code. A claim that the code does not support is a
defect in the document and, where it reveals a real gap, also a finding.

## Scope: which folders need these files

A folder needs both files when it contains a crate root (`Cargo.toml` with a
`[package]`) or is an ownership boundary named in the root `AGENTS.md` role
table.

At the reviewed commit the following crates have no `ARCHITECTURE.md`:
`shared/identity`, `shared/observability`,
`shared/transport`, `hosts/audiocap`, `hosts/input-helper`,
`hosts/windows/cp-ipc`, `hosts/windows/credential-provider`.
`hosts/capenc` and `tools/` have no `AGENTS.md`.

## ARCHITECTURE.md, required sections in this order

```markdown
# <Crate or area name>

<One paragraph: what this is, in the product's terms, and why it exists.>

## Purpose and scope

What this area is responsible for, and explicitly what it is not responsible
for. Name the sibling that owns each adjacent concern.

## Ownership and invariants

What this area owns exclusively. The invariants it maintains, stated as
falsifiable claims, each with the file that enforces it. If an invariant is
enforced by convention rather than by a type or an assertion, say so.

## Public surface

The types, traits, and functions other areas may use, and the stability
expectation for each. For binary crates, the CLI arguments, subcommands, exit
codes, config keys, and IPC contracts instead.

## Dependencies in and out

Two lists. What this depends on and why. What depends on this. State the
dependency-direction rule that applies from the root `AGENTS.md` and confirm
this area obeys it.

## Concurrency and blocking

The threading and task model. Which functions block, which are async, which must
not be called from an async context, which locks exist and in what order they
may be taken. If the area is single-threaded, say so.

## Error model

The error type or types this area produces, how they classify (retryable,
terminal, user-actionable, internal), and what the area does on each class.
Whether any path panics and under what precondition.

## Platform-specific notes

What differs by target OS and why the difference is necessary rather than
accidental. If a sibling platform implements the same concern differently, link
it and say which form is canonical.

## Known limitations

What does not work yet, what is deliberately out of scope, and what is
temporary. Each entry names the condition that would remove it.
```

## AGENTS.md, required sections in this order

The repository root `AGENTS.md` and existing folder files already carry role and
validation content. This standard preserves that content and fixes the ordering
and the missing sections.

```markdown
# <Area> Ownership

**Owner role:** <role from the root AGENTS.md role table>

<One paragraph: what this role owns in this path.>

## What you may change here

Concrete list. Files, modules, and behaviours that are in scope for a routine
change in this folder.

## What you must not change here

Concrete list. Shared APIs, wire formats, trust boundaries, cryptographic
material, dependency direction, packaging, and anything requiring escalation.
Name the role each item escalates to.

## Invariants that must not break

Falsifiable statements with the file that enforces each. These are the things a
reviewer checks first.

## Conventions

Naming, error handling, logging targets, telemetry field names, and module
layout rules specific to this area. Where a convention is repository-wide, link
`docs/architecture/LEXICON.md` rather than restating it.

## How to validate

The exact commands for this area, copy-pasteable, with the platform each must
run on. State plainly when an area cannot be built or tested on a developer
machine of a given OS.

## Gotchas

Non-obvious traps that have cost someone time. Each one states the symptom
first, then the cause, then the fix.

## Escalation

Who to notify for which class of change.
```

## Rules that apply to both

1. Every factual claim is checkable against the code at the current commit. No
   aspirational statements in the present tense. Roadmap items belong under
   "Known limitations" with the word "not yet".
2. Line and file references use repo-relative paths in backticks.
3. Plain, direct, active voice. No em-dashes. No marketing tone.
4. Do not restate the root `AGENTS.md`. Link it.
5. Do not duplicate `ARCHITECTURE.md` content in `AGENTS.md`. If an agent needs
   to understand the design, `AGENTS.md` links to `ARCHITECTURE.md`.
6. Sections are never omitted. If a section does not apply, keep the heading and
   write one line saying why, for example "This crate is single-threaded and has
   no locks."
7. Status statements that will age ("current state as of DATE") carry the date
   and the commit or release they describe, so a reader can tell staleness from
   fact.
