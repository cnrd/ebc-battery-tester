# EBC Battery Tester — Agent Instructions

This repository controls EBC battery testers, particularly the EBC-A20.

Before making changes:

1. Inspect the current repository and Git state.
2. Read the relevant existing implementation before editing.
3. Treat the repository as authoritative if this file has drifted in minor implementation details.
4. Preserve the architecture and hardware-validated behavior described below unless the user explicitly asks to change it.

Do not casually redesign settled architecture.

---

# Project architecture

The major architectural principle is:

> The GUI is only a GUI. All GUIs either connect to an independently running server or operate directly against a locally attached device.

The server/backend owns physical test execution.

A browser or remote GUI may:

- view state;
- request semantic operations;
- disconnect;
- reload;
- crash;

without changing an already-running physical test merely because the client disappeared.

There must be exactly one backend owner of the physical serial device.

Remote clients do not:

- own physical timers;
- infer physical lifecycle;
- advance cycle steps;
- write raw protocol frames;
- decide whether hardware actually started or stopped.

HTTP/backend commands express semantic intent.

The backend/controller and fresh device reports remain authoritative for physical state.

---

# Physical-state philosophy

A successfully issued command does not itself prove the hardware reached the requested state.

Lifecycle is conceptually:

```text
requested
-> command sent
-> hardware report confirms state
-> authoritative state changes
```

Persisted state after a process restart must never be treated as proof that the physical hardware is still active.

The project intentionally uses conservative uncertainty handling.

Do not introduce automatic ownership reclaim or automatic cycle continuation after process restart.

---

# EBC-A20 protocol facts

The real EBC-A20 hardware has been extensively validated.

Do not alter these facts without explicit instruction and new hardware evidence.

## Serial

Real hardware uses:

```text
9600 baud
8 data bits
odd parity
1 stop bit
```

That is:

```text
9600 8O1
```

Some external documentation suggested even parity, but actual tested hardware requires odd parity.

## Framing and parsing

Protocol framing/parser behavior has already been validated against real hardware.

Important characteristics include:

- fixed 19-byte inbound frames;
- parser recovery from garbage and misalignment;
- embedded `0xF8` handling;
- normal outbound XOR checksum;
- inbound firmware 3.0.2 accepting either normal XOR or the observed firmware quirk equivalent to `xor - 0xF0` when XOR is at least `0xF0`.

Do not simplify or remove the inbound checksum quirk.

## State bytes

Observed normal state bytes include:

```text
CC:
  idle      0x00
  active    0x0A
  finished  0x14

CP:
  idle      0x01
  active    0x0B
  finished  0x15

CV:
  idle      0x02
  active    0x0C
  finished  0x16
```

Observed natural completion is:

```text
Active
-> Finished with old/stale current
-> Idle with old/stale current
-> Idle with current 0
```

The final confirmed zero-current state is important for safe cycle progression.

A first Active report after Start may legitimately report current 0.

---

# TimerSync

While a test is authoritatively active, the backend sends:

```text
TimerSync = command 0x0A
```

once per minute.

The hardware timer minute counter uses base-240 representation and starts at 1.

Real hardware confirmed the first TimerSync approximately 60 seconds after Start and continued synchronization thereafter.

Do not alter TimerSync semantics casually.

---

# Stop, disconnect, and resume semantics

Important commands include:

```text
Stop        0x02
Disconnect  0x06
Continue    0x08 / 0x18 / 0x28 depending on mode
```

Semantics:

- Stop physically stops the operation.
- Disconnect clears PC/session ownership on the device.
- Continue resumes a previously stopped operation.

Back-to-back Stop + Disconnect has been validated on real hardware.

---

# Controller lifecycle invariants

The controller distinguishes an owned physical run from uncertain recovered device activity.

Conceptually:

```text
Starting + Active
-> RunningOwned

RunningOwned + Active
-> measurements and owned metrics update

RecoveredUncertain + Active
-> live device values update
-> owned-test metrics do NOT resume

RunningOwned + Finished
-> Completed

RunningOwned + Idle
-> Stopped

Completed remains latched across later Idle reports
```

TimerSync only occurs for a fresh confirmed active report while the run is owned.

Reconnect to an already-active physical device must never silently reclaim ownership.

Do not weaken these semantics merely to simplify UI behavior.

---

# Per-run metrics

Per-physical-run metrics are separate from whole-cycle telemetry.

Capacity and energy for a physical run must only represent reports belonging to an owned physical run.

Rest periods are never fake physical runs.

Cycle child runs carry:

```rust
CycleRunContext {
    execution_id,
    repeat_index,
    step_index,
}
```

Cycle child physical runs intentionally remain unnamed.

They are identified through their parent cycle and `CycleRunContext`.

Do not copy a cycle execution name into every child `RunSummary`.

---

# Cycle engine

The cycle engine is intentionally simple.

The recipe model is:

```rust
CycleRecipe {
    steps: Vec<CycleStep>,
    repeat_count: u32,
}
```

Steps are:

```rust
CycleStep::Device { ... }
CycleStep::Rest { ... }
```

Current Device-step completion is hardware-driven.

The only repeat construct is whole-recipe finite repetition.

## Explicit non-goals

The user explicitly rejected nested or conditional recipe logic.

Do not implement:

- nested recipes;
- conditional steps;
- branching;
- nested repeat blocks;
- expression languages;
- scripting;
- variables.

These are intentional non-goals, not unfinished features.

Software completion targets such as:

```text
stop after X mAh
stop after X Wh
stop at X% estimated capacity
```

are intentionally deferred because their semantics have not yet been designed.

Do not introduce them opportunistically.

---

# Cycle states and progression

Cycle states include approximately:

```text
Idle
Preparing
StartingStep
RunningStep
Settling
Resting
Stopping
Completed
Stopped
Interrupted
```

Natural Device-step progression is:

```text
hardware Finished
-> Settling
-> wait for a fresh confirmed inactive report with current == 0
-> advance to next step
```

Rest uses a backend-owned monotonic timer.

Communication gaps, uncertainty, unsafe write failures, or process restart while a cycle is nonterminal result in conservative interruption.

Do not automatically resume cycles after restart.

---

# Whole-cycle telemetry

Whole-cycle telemetry is implemented and hardware validated.

`CycleSample` represents the complete execution timeline and includes fields such as:

- execution ID;
- sequence;
- timestamp;
- elapsed milliseconds;
- repeat index;
- step index;
- cycle state;
- physical test state;
- device mode;
- activity-known;
- active state;
- voltage;
- current;
- device capacity;
- owned-test capacity;
- owned-test energy.

Cycle sequence:

- starts at 0;
- is monotonic for the entire execution;
- does not reset between steps;
- does not reset between repeats.

Cycle elapsed time is continuous across the entire execution.

Only normal device reports generate cycle samples.

Firmware reports do not.

Resting device reports are included in cycle telemetry.

Server-side cycle CSV is full resolution.

Frontend presentation history may be bounded/downsampled.

---

# Telemetry ordering

The established processing order matters.

Conceptually:

```text
1. TestController consumes the physical report.
2. CycleSample is recorded.
3. CycleEngine consumes the resulting physical state and transitions.
```

Therefore a boundary sample may intentionally contain:

```text
cycle_state = RunningStep
test_state  = Completed
```

before the cycle transitions to Settling.

This is intentional and must not be “corrected” by reordering telemetry after cycle state transitions.

---

# Persistence philosophy

Server persistence includes:

- current session metadata;
- per-run metadata;
- per-run CSV;
- whole-cycle CSV;
- cycle execution metadata sidecars;
- saved recipes.

Malformed durable data should not silently disappear.

Fail clearly where appropriate.

Existing older persisted data should generally remain readable with serde defaults/options where practical.

This persistence-compatibility preference is separate from HTTP API compatibility.

---

# WIP API compatibility policy

The current HTTP API was developed on the unreleased `wip` branch and is not yet a stable public compatibility boundary.

Therefore:

> Prefer clean API migrations over compatibility scaffolding for obsolete WIP request formats.

Do not add unnecessary compatibility mechanisms such as:

- dual raw/envelope request parsing;
- `#[serde(untagged)]` legacy request alternatives;
- obsolete legacy API tests;
- parallel legacy routes.

When an API shape changes, update the current server, native client, WASM client, tests, and documentation together.

Persisted user data should still remain compatible where inexpensive and sensible.

---

# Run and cycle execution names

Manual physical runs and cycle executions support optional human-readable names.

Names:

- are metadata only;
- are editable;
- may be cleared;
- need not be unique;
- never replace immutable IDs.

Cycle child physical runs remain unnamed.

Execution names are distinct from saved-recipe names.

The controller/protocol layer must remain unaware of human-readable names.

---

# Saved recipes

Saved named recipes are implemented.

Canonical model:

```rust
SavedRecipe {
    id: String,
    name: String,
    recipe: CycleRecipe,
    revision: u64,
    created_at_utc: String,
    updated_at_utc: String,
}
```

Rules:

- ID is immutable.
- Name is required.
- Names need not be unique.
- Revision begins at 1.
- Successful edits increment revision exactly once.
- Update/delete use optimistic concurrency with `expected_revision`.
- Stale mutations conflict instead of silently overwriting another client's edit.

Names are never identity.

---

# Saved recipe provenance

Executions started from a saved recipe capture:

```rust
SavedRecipeReference {
    id: String,
    name: String,
    revision: u64,
}
```

The execution also stores the complete `CycleRecipe` snapshot actually started.

A saved recipe is therefore a mutable template, not a live dependency.

Once an execution starts, any later:

```text
recipe rename
recipe edit
recipe revision increment
recipe deletion
```

must have zero effect on that execution.

The cycle engine must never dereference the saved recipe again after Start.

Ad-hoc cycle starts have:

```text
saved_recipe = None
```

Do not infer provenance by comparing recipe contents.

---

# Server-authoritative saved-recipe Start

Remote/server saved-recipe execution must be resolved by immutable recipe ID inside the server actor.

Conceptually:

```text
POST /api/recipes/{id}/start
```

with an optional execution name.

The actor:

1. resolves the saved recipe;
2. clones its current `CycleRecipe`;
3. captures `{id, name, revision}`;
4. starts the execution using that immutable snapshot.

Do not replace this with:

```text
client GET recipe
-> client sends recipe back to /api/cycle/start
```

That would introduce edit/start races and weaken provenance.

Keep this operation usable by non-GUI clients.

---

# Saved recipe persistence

Server recipes live under:

```text
<data>/recipes/<recipe-id>.json
```

Deleted recipe IDs remain reserved through tombstones:

```text
<data>/recipes/<recipe-id>.deleted
```

This prevents recipe ID reuse across process restart.

Do not remove that invariant without explicit instruction.

Direct/native/WebUSB saved recipes instead use persisted eframe application state.

Local and remote recipe libraries are intentionally independent.

Switching transports must never automatically merge or copy them.

---

# Portable recipe sharing

Portable recipe import/export is implemented.

Canonical shared type:

```rust
RecipeExport {
    format: String,
    version: u32,
    name: String,
    recipe: CycleRecipe,
}
```

Canonical constants:

```rust
RECIPE_EXPORT_FORMAT  = "ebc-battery-tester-recipe";
RECIPE_EXPORT_VERSION = 1;
```

Recommended filename suffix:

```text
.ebc-recipe.json
```

Portable files contain only:

- format;
- format version;
- recipe name;
- complete `CycleRecipe`.

They intentionally exclude:

- saved recipe ID;
- revision;
- persistence timestamps;
- execution IDs;
- execution names;
- run IDs;
- history;
- telemetry;
- execution provenance.

Import always means:

```text
create a new local saved recipe
```

with:

- fresh identity;
- revision 1;
- new local timestamps.

Duplicate names are valid.

Import never merges by name.

Currently one portable file contains one recipe.

Do not add multi-recipe bundle/archive formats unless specifically requested.

Use the shared `RECIPE_EXPORT_FORMAT` and `RECIPE_EXPORT_VERSION` constants in Rust code/tests that represent the canonical format rather than duplicating their literals.

Intentional invalid-format test strings are allowed.

---

# Remote recipe synchronization

The server owns the remote recipe library.

Remote clients:

- obtain the authoritative recipe list;
- receive recipe upsert/delete events;
- refresh the library on reconnect;
- use immutable recipe ID as the key;
- use revision to reject stale upserts.

Do not persist the remote server's recipe library into local GUI application storage.

A dirty recipe editor must not silently lose edits if another remote client modifies the source recipe.

Clean editors may refresh automatically.

Dirty editors should retain local work and become stale/conflicted.

---

# Cycle execution sidecars

Cycle execution metadata sidecars contain historical execution information including:

- execution ID;
- optional execution name;
- complete recipe snapshot;
- optional saved-recipe provenance;
- start timestamp.

Renaming an execution must preserve all other sidecar metadata.

Metadata changes must not rewrite cycle telemetry CSV.

Older minimal sidecars should remain readable using optional/defaulted fields.

---

# Browser independence

Closing, refreshing, disconnecting, or crashing a remote/browser client must not automatically:

- Stop;
- Disconnect the physical tester;
- interrupt a server-owned run/cycle;
- reset telemetry.

Only explicit user intent should alter an independently running server-owned physical operation.

---

# Local/direct behavior

Local direct mode and remote server mode should share semantic lifecycle/controller/cycle behavior wherever possible.

Do not duplicate hardware lifecycle logic in the GUI.

Local mode may use ephemeral execution/run IDs where persistent server history does not exist, but lifecycle and safety semantics should stay aligned.

---

# GUI philosophy

The GUI is presentation and user intent, not physical authority.

Keep conceptually separate:

```text
editable next-start draft
authoritative currently running state
```

Editing a draft must never silently mutate a running test.

Metadata changes should be explicit.

Mobile/browser use is an important use case.

Avoid desktop-only assumptions unless there is a platform-specific alternative.

---

# Future compatibility constraints

Some future clients may control the server without using the GUI.

Keep server-side semantic operations usable by non-GUI clients.

In particular:

- saved recipes must remain addressable by immutable ID;
- starting a saved recipe by ID must remain a clean server-side operation;
- server APIs should not depend on GUI state;
- authoritative physical/cycle state must remain server-owned;
- external clients must not need to reimplement cycle progression or physical lifecycle logic.

Do not implement future integrations unless explicitly assigned.

This section describes compatibility constraints only, not a roadmap.

---

# Environment

Primary development environment is CachyOS / Arch Linux.

Podman is available.

Docker must not be assumed.

When validating container images locally, prefer Podman unless the user explicitly requests something else.

The server container must remain headless.

Do not accidentally introduce GUI/windowing dependencies into server-only builds.

---

# Important build configurations

Keep the project working across:

- native GUI/direct;
- native remote GUI;
- server-only;
- remote WASM;
- WebUSB WASM;
- Trunk web build.

Typical verification includes:

```bash
cargo fmt --check
cargo test --all-features
cargo test --no-default-features --features server
cargo clippy --all-targets --all-features -- -D warnings
```

Run additional target-specific checks/builds requested by the task.

Do not claim a command passed unless it was actually run.

---

# Hardware testing policy

Do not exercise a real battery or EBC-A20 unless the user explicitly asks for hardware validation.

For higher-level features such as:

- metadata;
- saved recipes;
- import/export;
- history;
- UI;
- persistence;
- API changes;

prefer:

- unit tests;
- mock-server tests;
- persistence tests;
- protocol-frame assertions;
- integration tests that do not touch hardware.

Existing hardware-evidence directories/files must remain untouched and untracked unless explicitly requested.

---

# Already hardware-validated behavior

Real-hardware validation already exists for important functionality including:

- CC operation;
- CP operation;
- CV operation;
- natural hardware cutoff;
- manual Stop;
- Continue/Resume;
- Adjust;
- TimerSync;
- reconnect behavior;
- parser/checksum handling;
- ownership semantics;
- generic cycle progression;
- repeat boundaries;
- Rest behavior;
- whole-cycle telemetry;
- persistence/restart behavior.

Do not repeat real charge/discharge tests merely to validate unrelated UI or metadata changes.

---

# Agent workflow and subagents

For substantial tasks, use subagents to keep the primary agent context focused.

Prefer delegating self-contained work such as:

- repository exploration and locating relevant code paths;
- reviewing existing behavior and invariants;
- investigating tests and fixtures;
- checking API/client parity;
- independent code review after implementation;
- running or analyzing broad verification/build matrices;
- investigating a specific suspected bug or edge case.

The primary agent should retain responsibility for:

- understanding the user's requested scope;
- architectural decisions;
- integrating findings from subagents;
- resolving conflicting findings;
- making or coordinating the final implementation;
- final diff review;
- final completion report.

Avoid loading large amounts of exploratory output into the primary context when a subagent can investigate and return a concise summary with file/line references.

Do not use subagents mechanically for trivial or narrowly scoped changes where delegation would add more overhead than value.

When multiple subagents are used, give them non-overlapping responsibilities where practical.

Avoid having multiple agents independently edit the same files unless explicitly coordinating those edits. Prefer subagents for investigation and review, and keep conflicting implementation ownership centralized.

---

# Scope discipline

For every task:

1. Inspect the relevant code first.
2. Understand existing types and flows.
3. Keep the implementation narrowly scoped.
4. Preserve established invariants.
5. Add meaningful regression tests.
6. Avoid speculative features.
7. Do not refactor unrelated areas merely because they could be cleaner.
8. Report any discovered issue that materially affects the assigned task.

If a small adjacent cleanup is necessary for correctness, perform it and explain it.

Otherwise keep unrelated cleanup separate.

Do not begin unassigned features merely because they seem like logical next steps.

---

# Git workflow

Before editing, inspect:

```bash
git status
git branch --show-current
git log -1 --oneline
```

Work on the branch requested by the user, normally:

```text
wip
```

Do not assume a commit SHA from documentation; inspect Git.

After implementation:

1. run the required verification;
2. inspect the diff;
3. ensure unrelated files are not included;
4. commit the completed work;
5. push only when requested or when the assigned task explicitly says to push.

Never modify or commit existing hardware evidence unless explicitly instructed.

---

# Completion reports

After an implementation task, provide a concise but concrete report containing:

- files changed;
- important data/schema changes;
- important behavior changes;
- persistence/API implications;
- tests added;
- exact verification commands and results;
- commit SHA;
- whether the commit was pushed;
- unresolved concerns, if any.

Do not claim success for checks that were not actually run.

Do not hide failing or skipped validation.

---

# Explicit project non-goals

Unless the user explicitly changes these decisions, do not implement:

```text
nested recipes
conditional recipes
branching recipes
nested repeat structures
recipe scripting
expression languages
automatic cycle resume after process restart
software mAh/Wh/% completion targets
```

These are deliberate project decisions.

---

# Final rule

When implementing a new task:

> Preserve physical safety and backend authority first. Do not weaken already-validated hardware semantics merely to make UI, persistence, or API work easier.

Inspect the repository, follow the existing architecture, implement only the assigned scope, verify it thoroughly, and report exactly what changed.
