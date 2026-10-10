# Physical-authority model

## 1. Status, scope, and governing questions

This document is the normative architectural contract for EBC-A20 physical control. **MUST**, **MUST NOT**, **SHOULD**, and **MAY** indicate requirements, prohibitions, recommendations, and permitted choices. A recommendation may be departed from only with an explicit rationale; a requirement may be changed only by deliberately revising this contract and its behavioral tests.

The contract governs the physical backend, including the controller, cycle engine, report parser, receive queues, command writer, metrics, persistence recovery, and their public command boundaries. GUI capability flags describe these rules; they do not enforce them. Remote clients express intent to a server that independently owns its device. Native direct and browser WebUSB backends implement the same authority rules.

Every state MUST answer five independent questions:

1. **Knowledge:** What has the tester actually reported about activity, mode, and measurements?
2. **Validity:** Is that evidence intact, current, ordered, and from this connection? Is it causally eligible for this particular decision?
3. **Ownership:** Is the observed operation attributable to an explicit, successfully transmitted application operation with uninterrupted compatible evidence?
4. **Control:** Which ordinary or autonomous actions does that evidence and ownership authorize?
5. **Safety:** Which attempts to Stop or Disconnect remain available despite absent knowledge or ownership?

These questions MUST NOT be collapsed into one `running`, `connected`, `active`, or `can_control` flag.

### 1.1 Physical limits and established protocol behavior

There MUST be one application backend owner of a physical connection. Software ownership is an attribution and authorization rule, not proof that hardware obeyed a command, not a physical lock against front-panel intervention, and not authentication.

The operational EBC-A20 connection uses **9600 baud, 8 data bits, odd parity, one stop bit (8O1)**. Inbound frames are 19 bytes; outbound frames are 10 bytes. The application protocol has no transaction identifier or explicit command acknowledgement. Ordered compatible reports provide physical confirmation under the single-owner assumption; they cannot prove an unobservable firmware event or distinguish every same-mode external intervention.

Observed normal states are:

| Mode | Idle | Active | Finished |
|---|---:|---:|---:|
| Constant-current discharge (CC) | `0x00` | `0x0A` | `0x14` |
| Constant-power discharge (CP) | `0x01` | `0x0B` | `0x15` |
| Constant-voltage charge (CV) | `0x02` | `0x0C` | `0x16` |

CV charging's internal current-regulation phase is still protocol mode CV; it is not a CC-discharge mode transition.

Natural completion may be `Active → Finished with retained current → Idle with retained current → Idle with current 0`. An inactive status and zero measured current are different facts. Conversely, the first Active report after Start may legitimately have current 0. Implementations MUST preserve these distinctions.

Stop is `0x02`; Disconnect is `0x06`; Continue is `0x08`, `0x18`, or `0x28`; TimerSync is `0x0A`. Back-to-back Stop followed by Disconnect is supported. Disconnect relinquishes the device's PC session; it MUST NOT be represented as proof of physical Stop.

## 2. Terminology and minimal state model

The following are conceptual states, not a required public JSON schema. Equivalent internal representations are permitted if they enforce all transitions below.

### 2.1 Transport and write state

**Disconnected** means there is no usable physical command channel. **Connecting** means an attempt is in progress, with no inherited observation. **Connected** means the channel is available, not that physical activity is known. **Replaced** means the old connection has been retired, even if the same port or USB device is reopened.

A **connection generation** is a non-reused runtime identity for one physical connection attempt/session. It scopes input, partial buffers, prepared commands, writes, completions, and acknowledgements. Integer wrap MUST NOT permit an outstanding object to match a later connection.

A physical write is separately **prepared**, **queued**, **in progress**, **succeeded**, **failed**, or **outcome uncertain**. Queue acceptance is not write success. Serial success means the transport accepted the complete frame; USB success requires `Ok` status and the full byte count. Neither means the tester performed the requested operation.

### 2.2 Observation and validity

An **observation** is decoded information from an integrity-valid tester report, with physical receive provenance. An **ordinary report** identifies Idle, Active, or Finished. A **firmware report** includes identity/version information and physical mode, active/inactive, voltage, current, and raw capacity, but its inactive form does not distinguish Idle from Finished.

Observation conditions are:

- **None:** no eligible report from this connection.
- **Fresh valid:** accepted integrity, generation, receipt age, and ordering.
- **Stale valid:** structurally valid evidence whose authority interval has ended; historical only.
- **Invalid/corrupt:** failed framing, integrity, or supported report interpretation. It is not an observation with weakened authority.
- **Queued:** received but not yet applied. Queuing is neither validity nor freshness.
- **Recovered fresh:** fresh evidence after uncertainty. This restores knowledge, not previous ownership.

Validity has independent dimensions: framing, checksum form, supported field interpretation, connection identity, receipt-age bound, receipt order, and decision-specific causality. A parseable payload MUST NOT bypass any dimension. Rejecting a corrupt frame does not itself erase an earlier still-fresh valid observation; it simply cannot renew it. Transport failure or loss of trustworthy ordering does invalidate the affected evidence.

### 2.3 Physical knowledge

Physical activity is **Unknown**, **Active**, or **Non-active**. Non-active additionally distinguishes **Idle**, **Finished**, and **inactive with unspecified terminal reason**. **Settled** means a fresh valid non-active report with current exactly zero. Finished or Idle with retained nonzero current is **non-active but transitional/unsettled**.

The mode is separately known or unknown. A known mode can be **compatible** or **contradictory** relative to an acquisition, owned run, or cycle settling expectation. Contradiction is valid physical information, not corruption.

The supported EBC-A20 report forms all identify their mode. An unknown report type MUST NOT be guessed into one of those forms. If mode is unknown, it cannot establish/retain an expected-mode owner or satisfy a mode-dependent transition; cached intended mode is not a substitute. Activity information from any future partially understood form requires its own validated protocol definition before it can prove inactivity. Until then safety policy treats inactivity as unproven.

**Proven inactive**, for this contract, means a fresh current-generation non-active status, with no unresolved later energizing command. It is a bounded protocol observation, not an electrical measurement guarantee. A pending Start cannot be cancelled or considered safely over merely by observing another inactive report.

### 2.4 Physical ownership and limited closing attribution

**Physical ownership** answers whether an operation is still legitimately attributable to explicit application intent and may receive owned control. It consists conceptually of:

- **Unowned:** no operation authorization.
- **Acquisition pending:** an authorized Start/Continue has actually been written; no owned Active operation has yet been confirmed.
- **Manual-owned:** compatible Active evidence confirms the explicit manual operation.
- **Cycle-owned:** compatible Active evidence confirms a child operation of the currently executing cycle.
- **Revoked:** a formerly pending/owned operation lost its authority, with a recorded reason.
- **Recovered-unowned:** fresh evidence exists after revocation, but no new acquisition occurred.

Owned state MUST carry an operation identity, ownership epoch, expected mode/configuration, command provenance, and current observation relationship. Knowing the mode or recognizing an old run ID is insufficient.

A separate **closing attribution token** MAY retain the right to account one compatible final device-capacity reading or classify a terminal result after Stop or first inactive evidence. It is NOT physical control ownership. It authorizes no Active samples, elapsed-time restart, TimerSync, Adjust, or autonomous reacquisition. It expires on freshness loss, incompatible evidence, new operation, generation change, or finalization. This distinguishes a legitimate final counter from an accidentally retained running owner.

### 2.5 Orchestration ownership

**Orchestration ownership** is the exclusive right of an executing cycle to schedule its child work, including when no physical run is active. Preparing, StartingStep, RunningStep, Stopping, Settling, and Resting hold this reservation. Interrupted, Stopped, Completed, and absent/Idle cycles do not.

Rest owns scheduling, not a fictitious physical operation. Settling owns a bounded completion barrier, not the right to maintain a running output. Terminal cycle metadata and historical child association are not live authority.

### 2.6 Intent, authority epochs, confirmation, and safety authority

**User intent** is an explicit request. **Cycle intent** comes from a presently authorized cycle transition. **Authorization** validates that intent against live physical and orchestration state. A **prepared command** is an immutable authorization token and its exact optional frame. **No-frame semantic completion** acknowledges a policy result, such as bounded duplicate Stop, and MUST NOT claim transmission.

An **authority epoch** identifies a particular uninterrupted authorization context. Revocation or replacement invalidates its ordinary commands. An **operation epoch** prevents a command for one run/step from applying to another even if their visible states and configuration happen to match.

An application/HTTP **acknowledgement** reports acceptance or write outcome, as appropriate. **Physical confirmation** is a later eligible report establishing the relevant physical effect. These MUST remain distinguishable in backend state and error handling.

**Safety authority** permits an attempt to Stop potentially active hardware without owning it. It does not permit Start, Adjust, calibration, Continue, TimerSync, or ownership recovery. The presence of an actual Stop frame is essential to any special write/completion exemption.

## 3. Fundamental invariants

1. **Transport-open is not physical-state-known.** Opening, reconnecting, or repeating Connected status grants no observation or ownership.
2. **Valid observation is not owned operation.** Unowned Active is a meaningful and safety-relevant state.
3. **Freshness starts at physical ingress.** Parser, queue, backend, UI, HTTP, and persistence timestamps MUST NOT rejuvenate evidence. Unknown receive delay is handled conservatively (§4).
4. **Invalid reports grant nothing.** They cannot refresh knowledge, acknowledge commands, account metrics, or progress cycles; stream recovery remains enabled.
5. **Stale queued evidence never becomes fresh.** Age is tested against current monotonic time before applying it, including after suspension and delayed writes.
6. **Connections isolate authority.** No report, partial frame, command, completion, ownership, or Stop suppression crosses a connection generation.
7. **Ownership requires explicit transmitted intent and compatible later evidence.** Acquisition is bounded; inactive reports cannot keep an old Start eligible indefinitely.
8. **Write success is not physical effect.** Failure or late success may leave the hardware outcome unknown. Software MUST NOT manufacture confirmation.
9. **No-frame success is not a write.** It receives no real-Stop exemption and cannot renew a deadline or revive orchestration.
10. **Contradiction updates knowledge and revokes attribution.** A valid incompatible mode is retained as observation while owned authority is removed before metric decisions.
11. **Revocation is immediate and one-way within an epoch.** Owned actions, integration, clock advancement, and pending ordinary commands become ineligible before any further action.
12. **Recovery restores knowledge only.** Later expected-mode Active MUST NOT silently return ownership, including after an inactive terminal observation.
13. **Terminal cycles have no residual command jurisdiction.** They cannot block new manual work, intercept its Stop, resume themselves, or retain Stop suppression.
14. **Stop eligibility follows uncertainty, not run metadata.** If inactivity is unproven, explicit Stop is safety-eligible on a usable channel, including before the first report.
15. **Stop retry has a finite independent bound.** Neither silence, an absent freshness deadline, nor a successful write can suppress retry indefinitely. In-flight I/O must also have a bounded resolution path.
16. **Disconnect remains available.** Use bounded Stop-before-Disconnect when needed; failure to confirm or transmit Stop cannot indefinitely prevent retirement of the transport.
17. **TimerSync requires current owned Active authority.** It never creates, refreshes, or preserves that authority.
18. **Owned metrics require owned evidence.** Raw diagnostics, cycle timeline samples, closing attribution, and owned run samples are distinct. No integration crosses an unknown/unowned interval.
19. **Autonomous progression requires compatible current evidence.** Every executing phase, including Rest and Settling, is subject to expiry and activity contradictions. A timer alone cannot authorize new work.
20. **Orchestration exclusion is a backend rule.** Every manual energizing/control entry point obeys the same cycle reservation; UI flags are only a projection.
21. **Persistence cannot recreate live authority.** Restart retains history, never freshness, pending acquisition, ownership, command tokens, or Stop suppression.
22. **Restart/reconnect never silently resume physical work.** A fresh acquisition requires new explicit intent and new evidence.
23. **Snapshots cannot act as reports.** Cached device fields, repeated Connected, HTTP state, GUI state, and remote events cannot refresh the physical backend's authority.
24. **Authorization is checked at prepare, wire, and completion.** State/epoch changes invalidate ordinary commands; actual Stop frames have only narrowly defined exceptions.
25. **Already-received evidence precedes new ordinary action.** A backend MUST reconcile the available receive prefix before writing an ordinary command or committing autonomous progression; it cannot act on an earlier Idle while a later received Active is waiting (§4.4).
26. **An inactive observation closes Active control ownership.** Terminal-reason uncertainty may retain limited closing attribution, never dormant running authority that a later Active revives.
27. **Safety policy is shared.** Direct, server, and WebUSB execution differ in transport mechanics, not physical ownership or authorization semantics.

## 4. Observation lifecycle, receipt time, and causal order

### 4.1 Required lifecycle

```text
physical input becomes available at the transport receive boundary
  → capture connection generation and monotonic receive provenance
  → preserve provenance with partial bytes and complete receive batches
  → identify the fixed-length frame and supported report form
  → validate integrity before constructing an authoritative report
  → enqueue the validated report with its original provenance, if necessary
  → before applying: expire existing authority against current runtime time
  → check connection, receipt age, receipt order, and decision-specific fence
  → update physical knowledge and revoke incompatible ownership
  → decide metric/sample eligibility
  → apply cycle transition rules
  → reconcile the available receive prefix before issuing new physical actions
```

Framing and checksum work MAY occur before or after a raw-byte queue, but receive metadata MUST be recorded before parser work, copying delays, or deferred processing. No raw input becomes authoritative before integrity validation.

### 4.2 Normative clock and ingress uncertainty

The freshness interval is **10 seconds**, measured monotonically. At `now >= received_at + 10 s`, the observation is stale. Future-dated observations are invalid. Wall-clock UTC is for history labels only.

For a complete report delivered in one promptly serviced read/transfer, `received_at` is the time the transport receives that input, recorded before parsing. For a fragmented report it MUST conservatively represent the earliest constituent byte's receive time; the last fragment MUST NOT give an old prefix a new age or post-command identity. All frames already present in one receive batch retain that batch's input time, not separate processing times.

Portable serial and WebUSB APIs do not necessarily expose the time a UART byte arrived or a USB transfer completed inside the OS/browser. The application MUST NOT claim that callback execution time proves physical receipt time after arbitrary buffering. It MUST either retain trustworthy lower-layer timing, or use a conservative earliest-possible receipt bound and mark uncertain input as unsuitable for live authority. In particular:

- **Native and server serial:** record time immediately at input acquisition; preserve partial-buffer metadata. Reads MUST be serviced independently of long command/persistence work, or a receive-service gap must quarantine/drain potentially older driver-buffered input before recovering authority. A read made after a lifecycle write cannot, by itself, certify that all returned bytes arrived after that write.
- **WebUSB:** record provenance at the earliest runnable input completion boundary, before copying/parsing and before the backend queue. If a Promise may have completed during suspension, the resume callback's time is not a trustworthy new receipt time. Use an earlier conservative bound or discard/quarantine that transfer. An outstanding read spanning a lifecycle fence cannot establish post-fence causality merely because its continuation runs later.
- **Batching/fragmentation:** preserve input order and oldest-byte provenance through split/coalesced reads. Clear or quarantine partial frames spanning a lifecycle fence or receive-service uncertainty; completing their checksum later does not repair causality.
- **Suspend/resume:** expiry MUST precede every action on resumption. If the monotonic platform clock excludes suspend time, a suspension notification or conservative gap invalidation is required. Buffered input of indeterminate age cannot immediately restore authority.

These are software evidence requirements, not claims of an on-device timestamp or perfect knowledge of firmware buffering. Normal bounded serial transit is inherent in this protocol. Supporting a new bound or treating uncertain buffered input as current requires evidence, not a scheduling assumption.

### 4.3 Minimal envelope and fences

The conceptual queue item is:

```text
ReceivedReport {
    integrity_validated_report,  // retains ordinary/firmware kind
    connection_generation,
    received_at,                // conservative oldest-byte monotonic receipt bound
    receive_ordinal             // strictly increasing complete-report order
}
```

The ingress layer additionally retains provenance for partial frames and whether the receipt bound is trustworthy. Untrusted items MAY be diagnostic records; they MUST NOT be passed as valid `ReceivedReport` authority. Raw bytes and checksum-variant annotations are useful diagnostics, not additional authority.

An equivalent strictly FIFO representation may make `receive_ordinal` implicit. Thus `ParsedReport + generation + receipt timestamp` is sufficient **only** if integrity is guaranteed, FIFO order is preserved, partial input carries oldest-byte provenance, uncertain lower-layer backlog is handled, and a strict lifecycle fence is enforced. A timestamp field alone provides none of those guarantees.

For Start, Continue, and real Stop, maintain a **lifecycle receipt fence** at successful write completion (also after an uncertain late completion). Confirmation requires a report wholly received after that boundary on the same connection. A receipt time equal to the fence MUST be rejected for confirmation: coarse-clock ties are not proof of order. An implementation MAY admit a tied timestamp only if a shared ingress/write event sequence proves that the entire frame began after write completion. Independent parser counters do not provide that proof.

Generation mismatch is checked before changing any fence or live state. A new connection establishes its own fence. Adjust and calibration do not establish a new physical lifecycle and MUST NOT discard fresh contradictory observations queued during their writes.

### 4.4 Available receive prefix and action barrier

Before any ordinary wire action or autonomous transition is made effective, establish the finite ingress watermark already available to that backend and reconcile every valid item through it. A command selected from an earlier observation is provisional until this check. Later contradictions revoke ownership even if a still-later report returns to the expected mode. Merely choosing the last item and hiding intermediate revocation is forbidden.

This barrier MUST NOT wait indefinitely for future reports on a continuously reporting device. It drains an already-available prefix, then revalidates at the actual write boundary. Safety Stop may bypass this wait; it must remain promptly attemptable. Physical changes occurring after that boundary remain an unavoidable observation limit and are handled by subsequent reports.

| Receive schedule | Required result |
|---|---|
| Multiple fresh reports | Apply in receive order; account each eligible sample once; authorize new action only after the available prefix is reconciled. |
| Fresh and stale mixed | Stale items have no live effects. Expire old authority first; fresh recovery is unowned. |
| Active then Idle | Observe termination; do not infer that a later queued new Active belongs to the terminated epoch. |
| Idle then Active, already received before Start | Active defeats the Idle precondition. No Start may be emitted from that Idle. |
| Finished, settled Idle, then Active in one pre-next-step batch | The new Active contradicts Settling. Interrupt; do not emit the next child Start and merely call it unconfirmed. |
| Report received before Start, processed after Start | No acquisition confirmation, owned sample, or deadline renewal for that acquisition. Diagnostic retention is allowed. |
| Active/Idle received during Stop write | Cannot confirm the write's effect; no post-Stop owned sample. Await a later eligible report. |
| Old reports behind Adjust/calibration | Fresh ordered items still observe/revoke the existing operation; stale items do not. |
| Firmware queued | Same age/order/fence checks; no telemetry rows. |
| Queue crosses reconnect | Drop old-generation live effects, including errors and partial bytes; do not retag. |
| Equal coarse timestamps | FIFO preserves observation order; strict fence excludes ambiguous command acknowledgement. |

Pre-fence reports MAY be retained in diagnostic history with their original provenance. An implementation MAY discard them entirely. They MUST NOT replace a newer live observation or acknowledge later intent.

## 5. Normative authority matrix

The tables compose: apply the observation row, then the orchestration restriction, then the command-specific requirements. **Y** means eligible, not automatically executed. **N** means forbidden. **C** means the stated additional condition is mandatory. **S** means safety-eligible real Stop, subject only to bounded same-attempt deduplication (§8). All physical writes require a usable current-generation transport.

### 5.1 Physical observation/ownership matrix

| Meaningful state | Manual Start | Cycle Start | Continue | Adjust | Live calibration | TimerSync | Owned Active sample / capacity / energy / elapsed | Explicit Stop / retry | Disconnect / Stop first |
|---|---|---|---|---|---|---|---|---|---|
| Disconnected/connecting; unknown, unowned | N | N | N | N | N | N | N / N / N / N | Intent may be reported unavailable; do not queue for future connection | Y cancel/retire; attempt Stop only on still-usable old channel |
| Connected; none, stale, corrupt-only, or already-stale queue; unknown, unowned/revoked | N | N | N | N | N | N | N / N / N / N | S / S, including repeated silent requests | Y / required bounded attempt |
| Fresh valid non-active; no pending work; unowned | C | C | C | N | Voltage C | N | N / N / N / N | No physical frame required while inactivity proven | Y / optional redundant Stop |
| Fresh Finished or other non-active, current nonzero; unowned, no cycle | C | N until zero | C only stopped-resumable | N | Voltage C | N | N; closing counter only if token exists | No frame required unless unresolved energizing intent | Y / optional |
| Acquisition pending; none or compatible/prior-mode inactive | N | N | N | N | N | N | N / N / N / N | S / bounded retry | Y / required |
| Fresh expected Active; manual-owned | N | N | N | CC only | Voltage C; current CC only | Y | Y / Y / Y / Y | S / bounded retry | Y / required |
| Fresh expected Active; cycle-owned | N | N | N | N manual | N manual | C executing child | Y / Y / Y / Y | S / bounded retry | Y / required |
| Fresh Active, contradictory or otherwise unowned/recovered | N | N | N | N | N | N | N / N / N / N | S / S after contrary Active | Y / required |
| Mode unknown or required activity interpretation unsupported; no eligible complete physical observation | N | N | N | N | N | N | N / N / N / N | S / S while inactivity unproven | Y / required |
| Stop accepted/pending; Active or unknown; control ownership ended | N | N | N | N | N | N | N / N / N / N; bounded final counter token only | S / bounded retry | Y / required bounded attempt or same valid pending attempt |
| Fresh non-active terminal reason pending; closing token, no control ownership | N until terminal resolution or explicit cancellation | N | N | N | N | N | N / final counter C / no integration / frozen | C unresolved intent; otherwise no frame needed | Y / optional if inactive proven |
| Recovered fresh inactive after revocation | C new intent | C new execution | C explicit new acquisition | N | Voltage C | N | N until new acquisition | No frame required while inactivity proven | Y / optional |

Start requires a validated configuration, fresh non-active observation, live positive voltage, no pending energizing command, and no conflicting orchestration reservation. Explicit manual Start need not await a zero-current telemetry field when non-active status is proven; automated cycle Start requires zero current. Continue additionally requires a stopped resumable configuration and explicit new acquisition, never automatic continuation of an interrupted cycle. Its actually transmitted configuration defines the expected mode.

When inactivity is proven and no cycle cancellation is needed, an explicit Stop MAY return a clearly identified already-inactive/no-action result or an ineligible-state response. It MUST NOT claim a physical write. This narrow no-action case ceases to apply at freshness expiry.

Voltage calibration requires fresh live voltage and either an inactive maintenance state or a manual-owned compatible Active run. Current calibration requires manual-owned CC Active. Calibration is forbidden during pending acquisition, Stop, uncertainty, unowned Active, or cycle reservation. Confirm requires all four validated references staged in the same uninterrupted calibration/connection authority context; no backend-specific bypass is permitted. Calibration write success is not proof of the external reference's correctness.

Normal authorized Start/Stop operations used to obtain current references MAY retain calibration staging. Observation loss, contradiction, transport replacement, and uncertain write outcome clear that staging. A calibration context is therefore not identical to a single run's operation epoch.

The Active metrics columns assume an ordinary report for a sample row. Firmware can supply trusted metric inputs under §10 but produces no sample row. Elapsed time advances monotonically while owned Active authority remains fresh, bounded at the freshness deadline. Capacity counter normalization and numerical integration are not interchangeable operations.

### 5.2 Orchestration matrix

| Cycle phase | Physical ownership expected | Manual Start / cycle Start / Continue / Adjust / calibration | Settling progression | Rest expiry into work | Other autonomous cycle advance | Cycle timeline sample |
|---|---|---|---|---|---|---|
| None/Idle | none or manual | Physical matrix | N | N | N | N |
| Preparing | none; fresh settled precondition | N / N / N / N / N | N | N | C validated current execution | C ordinary observation |
| StartingStep | acquisition pending after actual write | N / N / N / N / N | N | N | C compatible post-write Active only | C ordinary observation, no fabricated owned values |
| RunningStep | owned child | N / N / N / N / N | N until eligible Finished | N | C compatible terminal evidence | C ordinary observation |
| Stopping | no continuing Active control; optional closing token | N / N / N / N / N | N | N | Only stop confirmation/interruption | C ordinary observation, frozen owned values |
| Settling | none; completed-child barrier | N / N / N / N / N | C later fresh same-mode non-active, current 0 | N | C only after barrier and receive-prefix reconciliation | C ordinary observation |
| Resting | none; scheduling reservation | N / N / N / N / N | N | C fresh settled evidence, uninterrupted Rest | C finite recipe/repeat only | C ordinary inactive observation |
| Interrupted/Stopped/Completed | none attributable to this cycle | Physical matrix; new manual work allowed | N | N | N | No further execution samples; separate diagnostics allowed |

Any nonterminal phase expires/interruption wins over a simultaneously due timer. Rest and Settling MUST interrupt on unexpected Active, even in the expected mode. Settling also interrupts on known incompatible mode. During Rest, mode alone has no expected physical-run relationship: a non-active mode change is not by itself a contradiction, but nonzero current violates the settled Rest condition and blocks progression. It MUST interrupt Rest rather than silently resume it later.

An invalid/corrupt incoming frame leaves the prior accepted state's row in force until its original deadline; it creates no new row. “Fresh owned with expired observation,” “cycle-owned with terminal orchestration,” and “unowned Active with owned metrics/TimerSync” are illegal live combinations. Recovery never makes them legal.

## 6. Acquisition and terminal evidence

### 6.1 Manual Start, child Start, and Continue

The required sequence is:

```text
explicit authorized manual intent, or authorized current cycle-child intent
  → fresh compatible precondition and orchestration guard
  → immutable prepared command for this connection/operation
  → receive-prefix reconciliation and wire reauthorization
  → full successful physical write
  → acquisition pending with post-write fence and finite acquisition deadline
  → fresh causally eligible expected-mode Active
  → owned operation
```

The acquisition deadline is **10 seconds after successful write completion**, and authority MUST expire earlier if its observation support or connection is lost. Inactive reports MAY refresh physical knowledge but MUST NOT extend this acquisition deadline. At equality, expiry wins. A delayed write completed after its original authorization expired MUST NOT establish pending acquisition; its possible physical effect is unknown/unowned.

This deadline is an application attribution policy, not a claim that the tester sends an acknowledgement in ten seconds. A device that has not confirmed by then remains observable and stoppable; later activation cannot be assigned to abandoned intent.

| Evidence/event while acquiring | Required outcome |
|---|---|
| First fresh expected Active, current 0 | Acquire; zero current does not mean inactive. |
| Wrong-mode Active, ordinary or firmware | Revoke acquisition immediately; known unowned Active; interrupt child cycle. |
| Prior-mode Idle/Finished/firmware-inactive | Observe non-active if causally eligible; remain pending without metrics; do not treat as completed Start or mode contradiction. Deadline still applies. |
| Corrupt, stale, pre-command, or tied-fence Active | No confirmation or metric effect. |
| Continued silence | Expire to unknown/unowned; interrupt cycle; retain Stop. |
| Continued inactive reports past acquisition deadline | End acquisition; known inactive/unowned if still fresh; interrupt cycle. |
| Replacement/disconnect/write failure | Revoke; no command replay or acquisition on replacement connection. |
| Expected Active after any revocation | Observe only; explicit new acquisition is required. |

### 6.2 Inactive and natural completion

Compatible ordinary Finished terminates an owned Active run as Completed; compatible ordinary Idle terminates it as Stopped. Completed MUST remain latched across subsequent Idle observations. Terminal capacity may be accounted using the closing token, but retained current MUST NOT be integrated as continuing power.

Compatible firmware-inactive immediately ends Active control authority and freezes elapsed/energy. It MAY leave the terminal reason unresolved until the next compatible ordinary report distinguishes Stopped from Completed. That report may resolve the child into Settling on Finished. A later Active without a new command is unowned and interrupts the cycle; it MUST NOT reactivate the old owner. While waiting for a terminal reason, inactivity is known, but Start/Continue remains blocked until resolution or explicit user cancellation of that pending semantic result.

For cycles, Finished enters Settling. A **later** eligible report (a greater receive ordinal, not necessarily a greater coarse timestamp) must confirm same-mode non-active and current 0. The Finished report itself MUST NOT simultaneously satisfy the barrier. Firmware-inactive can satisfy this later barrier because its inactivity/current fields are explicit. Unexpected Active or wrong mode interrupts it.

## 7. Revocation and contradictory modes

### 7.1 Revocation table

| Event | Physical knowledge | Physical/metric authority | Orchestration |
|---|---|---|---|
| Freshness expires | Unknown; retain historical fields only | Revoke pending/owned and closing token; freeze elapsed at deadline; break integration | Interrupt every nonterminal phase |
| New/replaced connection, process restart, explicit Disconnect | Unknown on new/retired connection | Revoke all live authority and pending tokens | Interrupt; history survives |
| Fresh incompatible mode during owned Active or closing attribution | Known reported activity/mode | Revoke before attribution; no contradictory final counter | Interrupt active child/Settling |
| Fresh unexpected Active during Rest/Settling/terminal-reason wait | Known unowned Active | No acquisition; revoke any residual closing token | Interrupt |
| Lost receipt ordering or command causality | Affected evidence cannot establish current state; usually Unknown | Revoke affected acquisition/owner; break metrics | Interrupt dependent execution |
| Acquisition deadline, denied ordinary completion | Keep independent valid observation, if any; physical effect of late write unconfirmed | No ownership; no metrics from abandoned intent | Interrupt child |
| Write failure/timeout/short transfer/stall | Unknown unless independently established after failure on a still-trusted channel | Revoke; retire uncertain transport as required | Interrupt |
| Explicit Stop accepted | Prior observation remains historical/current as its age allows; effect unconfirmed | Cancel acquisition/Active control immediately; optional closing token | Cancel progression; Stopping or terminal Interrupted stays latched |
| Compatible inactive/Finished | Known non-active, possibly unsettled | End Active control; finalize or retain limited terminal token | Stopped/interrupted on unexpected Idle; Settling on natural Finished |
| Cycle interruption for any reason | Preserve independently valid physical observation | Revoke that execution's child ownership, including pending commands; no metrics/TimerSync | Terminal Interrupted; no auto-resume |
| Invalid isolated report | Prior valid observation unchanged until deadline | No new authority; existing authority lasts only to original deadline | No progression from invalid input |

No report-age loss or contradiction alone automatically emits Stop under this policy. They revoke autonomy and expose explicit safety actions. Automatically stopping on these events would be a separate deliberate policy change, not a prerequisite for conservative uncertainty.

### 7.2 Exact mode contradiction behavior

For intended/owned `CC @ 100 mA` followed by fresh valid `CP Active`:

- Observation, activity, and CP mode are known and fresh.
- CC ownership and all acquisition/finalization rights are revoked.
- The manual run becomes uncertain/interrupted, not completed by CP data.
- An executing cycle becomes Interrupted and loses scheduling authority.
- No new owned sample, capacity, energy, or owned elapsed increment is attributed to that report. Existing totals are retained.
- TimerSync, Adjust, Continue, and live calibration are forbidden.
- Explicit Stop is safety-eligible; Disconnect uses bounded Stop-before-Disconnect.

The same rule applies to all six directed mode pairs: CC→CP, CC→CV, CP→CC, CP→CV, CV→CC, CV→CP, on first Active and established Running, manual and cycle, ordinary and firmware.

The owned clock freezes at the contradiction's receipt boundary. Time already legitimately projected up to that boundary is not a metric increment from the contradictory measurement; no time after that boundary may be attributed to the old owner.

A contradictory Finished/Idle/firmware-inactive report during an established owner still revokes attribution, but establishes non-active physical knowledge. It MUST NOT mark the intended run naturally Completed or satisfy its cycle barrier. A subsequent explicit new Start may be eligible after cycle interruption and suitable inactive preconditions; this is new intent, not reclaim. Inactive prior-mode reports during acquisition have the narrow exception in §6.1.

Later expected-mode reports MUST NOT undo the revocation. Mode compatibility tests the protocol operation class, not exact measured current (which may be zero/tapering), and not unvalidated assumptions about parameter echoes.

## 8. Safety Stop and Disconnect

### 8.1 Initially unknown and stale connections

**Yes: if physical inactivity has not been proven, explicit Stop is always safety-eligible on a usable physical channel.** This includes immediately after connection, after initial timeout, stale previously inactive state, and reconnect without reports. No owned test or fake run is needed.

Start, Continue, Adjust, calibration, TimerSync, owned samples, and cycle progression remain unavailable. A real Stop attempt does not change Unknown into Inactive. After a successful Stop write with continued unknown observation, each later explicit Stop may retransmit as soon as no actual write is in flight. A consumed freshness deadline is irrelevant to that permission.

### 8.2 Stop state machine

```text
safety eligible
  → explicit intent accepted (ordinary control/progression cancelled)
  → real Stop prepared
  → queued for this connection
  → write in progress
  → full write success → effect unconfirmed
       ↘ failed/timeout → outcome unknown; suppression released
effect unconfirmed
  → eligible later non-active report → inactive confirmed
  → eligible later Active → retry immediately eligible
  → deduplication deadline/observation expiry → retry eligible
```

Acceptance, preparation, queueing, wire progress, wire success, and physical confirmation MUST be separately identifiable internally. HTTP/UI success MUST NOT be used to shortcut the last transition. Safety Stop never acquires a run, starts a clock, or resets history.

Repeated intent MAY share one **genuinely pending** same-connection Stop attempt while it is queued/in flight. After write success, no-frame deduplication is allowed only while the same Stop attempt remains unconfirmed, the relevant prior observation is still fresh, no later report has evaluated the attempt, and no newer operation has superseded it.

Deduplication MUST have a finite monotonic bound independent of future input. Its maximum is **10 seconds from acceptance**; the original observation's freshness expiry may end it earlier. A no-frame completion MUST NOT extend either bound. Unknown observation has no post-write deduplication interval: subsequent explicit requests produce real retries. A new Active report ends post-write suppression immediately. Failure, connection replacement, or new operation clears it.

A separate pending/retry deadline is the preferred representation. Deriving an earlier bound from an existing observation deadline is equivalent only if missing/consumed observation deadlines mean **no post-write suppression**, never indefinite suppression. Stop does not need automatic retransmission: the requirement is bounded eligibility and servicing of explicit retry.

Every physical write attempt MUST have bounded host-side handling (at most 10 seconds while the executor is runnable). A blocked USB Promise or serial write MUST NOT indefinitely trap Stop/Disconnect behind it. On timeout, the executor must revoke authority, retire/quarantine the uncertain connection, release semantic suppression, and ignore later completions for live effects. It MUST NOT race a second ordinary writer on the same uncertain channel. A close attempt also requires bounded handling; unresolved cleanup remains visible as a transport error.

This cannot make JavaScript run while a browser/process is frozen. On resumption, elapsed bounds and cancellation MUST be evaluated before servicing stale completions or new ordinary work.

### 8.3 Retry cases

| Situation | Stop behavior |
|---|---|
| Fresh owned Active, immediate duplicate | One pending attempt/no-frame acknowledgement within bounds; no false inactive state. |
| Fresh later Active after successful Stop, any mode | Real retry eligible; no ownership restoration. |
| Stale Active or initially unknown, successful write then silence | Every later explicit request may write once the previous actual I/O resolves. |
| Interrupted/Completed cycle | Global safety policy; terminal cycle cannot suppress a manual operation's Stop. |
| New unrelated manual operation | New operation identity; discard previous operation's suppression. |
| Pending write never resolves | Timeout/retire; no indefinite command starvation; no falsely successful retry. |
| Write failure | No suppression; conservative transport error. Reconnect is new unowned state if needed. |
| Reconnect | Old attempts/completions dropped; new explicit Stop available before reports. |
| Fresh non-active confirmation | Mark physical inactivity known, release attempt; later stale state restores Stop eligibility. |

### 8.4 Conservative Disconnect

An explicit physical Disconnect cancels orchestration and ordinary pending work immediately.

1. If inactivity is proven and no unresolved energizing intent exists, disconnect directly. A redundant Stop is permitted.
2. Otherwise make a bounded real Stop attempt first, or share a genuinely in-flight same-connection Stop attempt within its bound. Unknown-state post-write no-frame suppression is not a substitute.
3. A successful Stop write permits proceeding with Disconnect **without waiting for physical confirmation**. Mark the final physical outcome unknown if no eligible inactive report confirmed it.
4. A failed/timed-out Stop does not indefinitely block Disconnect. If the channel remains usable, attempt protocol Disconnect; otherwise retire/close it and report the failures. Sending bytes on a channel already retired after failure is not required.
5. Local resource release MUST be attempted even when protocol Disconnect fails. Protocol Disconnect, host close, and physical Stop are distinct outcomes.

Persistence/publication failures MUST be reported but MUST NOT prevent this bounded safety cleanup. Cleanup must not depend on successfully flushing telemetry or writing metadata first.

Never wait indefinitely for an inactive report, write Promise, or device close. The bounded attempt policy does not promise the hardware has stopped. Remote GUI/network disconnect alone MUST NOT initiate this physical sequence on an independently running server.

## 9. Shared command lifecycle and orchestration boundary

### 9.1 Guard placement

One GUI-independent physical-authority boundary MUST combine controller authority with cycle reservation. It is used by native direct, server actor/HTTP, WebUSB direct, machine API, and any command-capable WebSocket entry point. Remote clients call that server boundary rather than reconstructing policy. The existing WebSocket observation stream is not itself an authority source.

At every manual Start/Continue/Adjust/calibration entry point, check the cycle reservation **before** replacing run metadata/history or generating a physical frame. Only a token tied to the currently executing cycle ID, step, repeat, and operation epoch may authorize child Start. Merely passing nonempty historical `CycleRunContext` is not a bypass credential.

Capabilities MUST be derived from these same guards, including safety Stop and Disconnect. A missing or incorrect GUI restriction cannot enable a backend command. Historical terminal cycles MUST release their pending actions and stop influencing new commands.

### 9.2 Three authorization points

| Point | Mandatory decisions |
|---|---|
| Prepare | Expire authority; reconcile available evidence; validate user/cycle origin, configuration, observation, cycle reservation, connection and operation epoch; select exact frame or explicit no-frame result. |
| Immediately before physical write | Verify the same connection, operation, immutable parameters, current observation age/mode and cycle reservation; reconcile the ingress watermark; reject superseded tokens. Validate actual frame identity for safety exemptions. |
| Immediately before semantic completion | Reject old-generation/duplicate/cancelled completions before mutating state; expire authority; verify token and operation epoch; record write outcome separately from confirmation; establish lifecycle fence where required. |

An ordinary command that loses authorization before writing MUST emit no bytes. If it was already written when authorization was lost, its effect is uncertain and completion MUST NOT regain ownership. Failure to authorize a queued child action also clears that pending action and interrupts its cycle; it must not leave an immortal StartingStep.

A real Stop frame can be written and its transmission recorded after observation/ownership expiry, provided it still belongs to the same connection and unsuperseded safety attempt. This does not revive metrics or a cycle. The exemption MUST be attached to `OutboundFrame::Stop`, not merely `CommandKind::Stop` or `frame.is_some()` in a generic interface. No-frame Stop results remain dependent on their original attempt and deadline.

Adjust/calibration successes record requested configuration/staging, not proof of physical effect. They preserve the existing lifecycle only if authority is still valid. Fresh contradictory observations received during their writes MUST be reconciled before reporting a continuing owned result. TimerSync follows the same wire authorization and generation rules, even if it has no public semantic command.

## 10. Telemetry and metrics authority

Physical observation, diagnostic history, cycle timeline, and owned run metrics are separate products.

| Input/event | Live observation | Owned run row | Cycle row | Owned metric/time effect |
|---|---|---|---|---|
| Fresh compatible owned ordinary Active | Yes | Yes | Yes if executing | Capacity/energy eligible; owned clock advances |
| First causally eligible expected Active | Yes | Yes if ordinary | Yes if ordinary child | Acquire; initialize new segment; no integration from pre-Start data |
| Fresh contradictory Active | Yes | No | Optional final diagnostic boundary row, clearly unowned | Freeze owned totals; revoke before row decisions |
| Recovered/unowned Active | Yes | No | No after interruption; diagnostic history allowed | None |
| Stale queued, corrupt, wrong-generation, pre-command-for-this-operation | No new current authority | No | No live execution row | None |
| Fresh firmware Active | Yes | No | No | May acquire/maintain compatible ownership and supply trusted metric inputs; no synthetic sample row |
| Compatible fresh Finished/Idle ending owned run | Yes | No Active row | Ordinary boundary row allowed | Final capacity via token; stop clock, break energy interpolation; no retained-current power integration |
| Compatible firmware-inactive | Yes | No | No | End Active ownership; terminal classification/one closing counter only |
| Fresh Rest inactive observation | Yes | No | Yes if ordinary and execution still active | No physical-run metrics; cycle timeline only |
| Inactive zero after Settling | Yes | No | Ordinary boundary row allowed | Barrier progression; no new run until authorized Start |

Firmware sampling exclusion does not make its validated voltage/current/capacity fields fictitious. These fields MAY update owned counters/integration while it legitimately confirms owned Active. Implementers choosing this behavior MUST apply exactly the ordinary ownership and gap rules; tests must distinguish “no row” from “no metric update.” Firmware never supplies an ordinary Finished reason.

Processing order is **controller knowledge/revocation → cycle sample decision → cycle transition**. A legitimate boundary sample may show `cycle_state=RunningStep` and `test_state=Completed`. A diagnostic contradiction boundary row MUST show the revoked/uncertain test state and frozen owned totals; it MUST NOT be presented as an owned child-run sample. Further rows after terminal interruption belong only to separate diagnostics.

Elapsed run time starts at the confirming Active receipt, not queue dequeue or write acceptance. It may project forward while authority remains fresh, but on delayed expiry freezes at the deadline, not the later tick. First inactive freezes it at that observation's receipt. Cycle timeline elapsed is a separate monotonic execution clock and includes Rest; terminal duration MUST be frozen. Sample chronology SHOULD retain receipt timing separately from persistence/publication timing.

Capacity is a device cumulative counter, not host current integration. The server's base-240 raw counter has a 57,600 mAh modulus; plausible wraps may be normalized only within attributable intervals. New Start resets owned totals. Explicit Continue after a stopped/recovered interval creates a new attribution segment, rebasing from confirmed pre-Continue inactive raw capacity; unowned counter growth MUST NOT be added. If a trustworthy baseline is absent, begin the new segment at its first eligible Active and do not invent the missing increment.

Server energy is trapezoidal integration between attributable voltage/current endpoints using monotonic elapsed receipt time. Break the interpolation endpoint on inactivity, Stop, expiry, contradiction, transport change, or restart. Recovery alone never bridges a gap. Duplicate/equal-time reports do not add time or energy.

Direct mode MAY retain a clearly identified voltage-times-capacity estimate instead of server-integrated Wh. Its algorithm is not numerically interchangeable with integrated Wh, but its **ownership domain must be identical**: raw capacity gained during unowned operation cannot be relabelled owned capacity/energy on Continue. Historical values MAY be retained and labelled incomplete; they MUST NOT be recomputed from unowned raw totals as though the interval were owned.

## 11. TimerSync authority

TimerSync is a physical command, not a harmless UI timer. Before scheduling and immediately before writing it, require:

- a usable current-generation connection;
- fresh, valid, causally current Active observation;
- confirmed manual ownership or ownership of the currently executing cycle child;
- expected compatible mode and Running lifecycle;
- no accepted Stop, pending termination, cycle interruption, or revocation;
- command token bound to this operation/connection.

Transmit at most one current eligible minute update when the owned elapsed clock crosses a minute. The first value is 1 at approximately 60 seconds of confirmed running time; encode the minute in base 240 within the protocol maximum 57,599. Do not replay a burst of skipped minutes to catch up. After revocation there is no catch-up until an explicit new acquisition; Continue preserves only legitimate prior owned elapsed time.

Freshness loss, contradiction, firmware-inactive, Stop, interruption, reconnect, and ownership revocation suppress TimerSync. Recovery, TimerSync write success, or a minute counter MUST NOT establish ownership or refresh observation. If expiry and a minute boundary coincide, expiry wins. A queued TimerSync must be cancelled when its operation ceases to own authority.

## 12. Firmware and protocol integrity

For inbound bytes `b[0..19)`, require `b[0]=0xFA`, `b[18]=0xF8`, and a supported report type in `b[1]`. Let `x` be XOR of the 16 payload bytes `b[1..17)`. Accept checksum `b[17]` only if:

```text
b[17] == x
    OR
(x >= 0xF0 AND b[17] == x - 0xF0)
```

The second form is the validated inbound firmware-3.0.2 high-XOR quirk; it is not a waiver for arbitrary checksum failures and is not restricted solely to the firmware-report message class. Outbound frames use ordinary XOR of their seven payload bytes. Do not add wrapping subtraction, arbitrary modulo variants, or “warn and accept” without new protocol evidence and a deliberate contract update.

Firmware Active types are CC `0x6E`, CP `0x6F`, CV `0x70`; firmware inactive types are `0x64`, `0x65`, `0x66`. Their explicit physical fields may refresh observation, confirm compatible acquisition, revoke contradictory ownership, and satisfy a later Settling non-active-zero barrier. They do not create run or cycle sample rows. Stale queued firmware is stale evidence just like ordinary input.

Framing MUST use the validated fixed length and recover after corrupt/garbage/misaligned bytes, including embedded `0xF8`. Unknown report types or uninterpretable required physical fields cannot authorize control. Resynchronization may scan for a subsequent valid frame; a failed frame MUST NOT be returned to the authority boundary as accepted. A checksum is an error-detection mechanism, not a cryptographic guarantee or evidence of ownership.

## 13. Connection replacement, restart, and persistence

On every new/replaced physical connection:

- current observation and its freshness become absent;
- pending acquisition, physical ownership, closing tokens, calibration staging, and Stop suppression are cleared;
- ordinary prepared commands and all old completions become unusable;
- active cycles become Interrupted, with no pending step or Rest continuation;
- owned clocks freeze and integration endpoints are cleared;
- historical values/identity/configuration may remain visibly historical;
- the new connection begins Unknown/unowned and permits explicit safety Stop.

Old-generation queued reports, partial bytes, errors, real-Stop completions, no-frame completions, TimerSyncs, and application acknowledgements MUST NOT mutate or revoke the replacement session. No physical command crosses the boundary. A user wanting Stop on the replacement connection submits new intent. Reopening the same path/device does not preserve generation.

Process persistence MAY retain historical runs/samples, cycle recipes and execution metadata, interrupted/completed results, names, immutable IDs, saved-recipe provenance, configuration, and installation UUID. It MUST NOT restore as live authority any monotonic freshness timestamp, connection generation, owned/pending operation, queued input authority, prepared command, receipt fence, or pending Stop suppression. Persisted physical mode/activity must be explicitly historical/unknown until new observation.

Startup maps formerly nonterminal work to conservative interrupted/uncertain history before opening a connection. No persisted Active marker, child context, server UUID, recipe, cached snapshot, or automatic transport reconnect authorizes Start/Continue or reclaim. A later explicit Continue is new acquisition, not restored authority.

## 14. Backend parity and architectural responsibility

The shared policy boundary owns receipt admission, expiry, knowledge, ownership acquisition/revocation, terminal attribution, command permissions, Stop state, cycle reservation, and TimerSync eligibility. The cycle engine owns only recipe sequencing and monotonic Rest timing under that boundary. It must consume explicit observation/ownership outcomes, not infer physical authority from presentation `TestState` alone.

Transport adapters own serial settings/USB setup, physical input provenance and buffering, actual write result, transport cancellation/timeout, and generation tagging. They MUST NOT independently implement lifecycle or Stop policy. The server additionally owns persistence and publication; the local backend additionally owns direct-mode event delivery. Neither duplicates policy for convenience.

The minimum useful consolidation is a shared authority coordinator around the existing controller and cycle engine, or equivalent shared guard/transition functions. A wholesale rewrite or a new transport framework is not required. Internal typed observation and operation tokens SHOULD make invalid combinations hard to construct. Snapshot DTOs SHOULD remain presentation/history types, not inputs capable of granting authority.

Remote/browser/native consumers receive backend capabilities and state, display uncertainty, and submit semantic intent. Client disappearance, discovery change, or remote transport resynchronization MUST NOT Stop or Disconnect independently running server-owned work. Direct WebUSB browser suspension, by contrast, suspends the physical executor itself and follows the ingress/expiry rules above.

## 15. Defect classes explained by the model

| Defect class | Violated invariants | Architectural cause | Shared preventive rule / regression family |
|---|---|---|---|
| Silent Rest later starts | 3, 5, 19 | Scheduling treated as independent of physical authority | Expiry before every phase tick; no recovery continuation / phase-deadline tests |
| Stale report retains authority | 3–5, 11 | Cached observation treated as timeless | Receipt-bound admission and inclusive deadline / observation boundary |
| No-frame repeated Stop swallows expiry | 9, 15, 24 | Semantic Stop mistaken for actual safety transmission | Frame-specific exemption; no-frame completion epoch check / Stop completion |
| Interrupted cycle suppresses later Stop | 13–15, 20 | Historical execution retains command jurisdiction/latch | Terminal reservation release; operation-scoped Stop / old-cycle-new-run public tests |
| Manual HTTP Start during Rest | 20, 24, 27 | Guard lives only in GUI or alternate command route | One backend guard before metadata/wire / HTTP and direct entry points |
| Stop after expiry never becomes retryable | 14–16 | Stop suppression relies on absent future report expiry | Independent bounded attempt; unknown post-write retry / silent Stop tests |
| Invalid checksum advances cycle | 4, 19 | Parsed structure confused with validated observation | Integrity-valid ingress type / corrupt-frame-to-wire tests |
| Initially unknown connection rejects Stop | 1, 14 | Stop requires remembered run metadata | Safety eligibility from unproven inactivity / empty-session Stop |
| Delayed queued WebUSB report becomes fresh | 3, 5, 25 | Receipt provenance lost at asynchronous queue | Preserve ingress provenance and fence / real Promise queue tests |
| Contradictory mode retains metrics/TimerSync | 2, 10–12, 17–18 | Only orchestration sees contradiction | Revoke at shared observation boundary before attribution / all-mode pairs |
| Energy interpolation across silence | 11, 18 | Integration endpoint outlives attribution | Segment endpoints cleared on loss / deterministic gap and persisted metrics |

These are failures to distinguish evidence, attribution, and authorization, not separate exceptions to a single `running` flag.

## 16. Proactive edge-case requirements

| Case | Required behavior |
|---|---|
| Manual contradiction on first Active or during Running | Known unowned activity; reject owned controls/metrics/TimerSync; safety Stop remains. |
| Contradictory Finished or inactive during established ownership | Record non-active mode; revoke attribution before final counter; do not complete intended cycle step. |
| Contradictory firmware | Same revocation as ordinary evidence; no sample row. |
| Expected mode returns after contradiction | Remain unowned; no automatic reacquisition. |
| Stale queued Active around Stop | No attribution, confirmation, or freshness restoration; retry remains safety-eligible. |
| Stale queued Idle around Stop | Cannot prove Stop or inactivity; do not suppress retry. |
| Pre-Start Idle processed after Start | Cannot settle acquisition or supply a new precondition; remain pending within deadline. |
| Pre-Stop Active processed after Stop | Cannot acknowledge later Stop or restore owned running; preserve only diagnostics. |
| Reports with mixed ages | Process eligible receive order; no stale item causes transitions; recovery after expiry unowned. |
| Coarse timestamp ties across command | Reject acknowledgement absent shared total-order proof. |
| Partial frame spans command | Retain oldest-byte provenance or discard partial frame; its completed payload cannot acknowledge command. |
| Newer received Active behind Idle/settling-zero | Reconcile it before next Start; reject/interrupt rather than writing from older evidence. |
| Reconnect with queued input | Old generation cannot restore observation; partial bytes cleared/quarantined. |
| Old-generation Stop completion | Ignore live effects before touching fences, new observation, new Stop attempt, or new cycle. |
| Initially unknown plus repeated Stop | Real attempts without creating run/ownership; only actual bounded in-flight attempt may coalesce. |
| Unknown Stop success then silence | Effect remains unknown; subsequent explicit retry available independently of reports. |
| Unknown Stop failure | Error/uncertainty, no suppression; retire channel when untrustworthy; fresh reconnect still unowned. |
| Stop success then contradictory Active | Observe potentially active unrelated mode; revoke closing attribution; allow real retry. |
| Disconnect while Stop unconfirmed | Proceed after bounded attempt without pretending inactivity was confirmed. |
| Cycle Interrupted while tester still Active | Known unowned Active; no TimerSync/metrics/step progression; global safety Stop. |
| Terminal cycle plus new manual run | New operation epoch; historical cycle cannot intercept controls or Stop. |
| Contradiction during Settling | Wrong mode or any Active interrupts; later same-mode zero cannot resume. |
| Contradiction during Rest | Any Active or loss of settled condition interrupts; inactive mode change alone has no owned-mode meaning. |
| TimerSync boundary coincides with loss | Revoke first; no frame. In-flight write may already have happened, but completion grants nothing. |
| Browser freezes with output Promise blocked | On resume expire/timeout before continuation; discard stale/uncertain input; no late command acquisition. |
| Browser freeze plus reconnect | New generation; old input and late writes/completions have no new-session effect. |
| Bytes arrived before command, callback/parser runs later | Timing of processing cannot prove causality; quarantine uncertain ingress or use trustworthy original provenance. |
| Firmware-inactive followed by same-mode Active | Old Active ownership ended; new Active is unowned, no silent metric/TimerSync restart. |
| Inactive reports continue after Start deadline | Knowledge may stay fresh; pending acquisition still expires and later Active is unowned. |
| Continue after unowned capacity growth | Explicit new ownership segment; exclude unowned raw increment and break energy interpolation. |

## 17. Minimum permanent acceptance-test families

Tests MUST assert forbidden wire actions as well as allowed state changes where a physical command path is involved. Controller tests alone cannot establish public-boundary or transport scheduling properties.

### Observation and integrity

- Exact `deadline − ε`, equality, and `deadline + ε`; invalid/future receipt time; initial no-report timeout; repeated Connected and snapshot replacement.
- Ordinary and firmware corruption, both accepted checksum forms, single-bit changes, garbage/embedded terminator/fragment recovery.
- Delayed complete queues, mixed ages, ordered batch contradictions, partial-frame age, receive-service gaps, and pre-command input fences including timestamp ties.
- Generation replacement with input, partial buffers, and error events pending.

### Ownership and terminal evidence

- Normal manual/child/Continue acquisition, first Active with zero current, prior-mode inactive pending Start, finite acquisition despite continuing inactive reports.
- All directed CC/CP/CV contradictions on first Active and established Running, ordinary/firmware, plus contradictory Finished/inactive.
- Expiry, recovery, no silent reclaim, firmware-inactive→Active, cycle interruption revoking child control, final counter attribution without retained-current integration.

### Stop and Disconnect

- Initially unknown, stale inactive, stale active, repeated silent Stop, fresh duplicate/no-frame Stop, real Stop crossing expiry, failed/partial/stalled Stop, and retry after continued silence.
- In-flight attempt that never resolves; bounded Stop/Disconnect servicing; late completion after cancellation; failed Disconnect still attempts local close.
- Interrupted/terminal old cycle followed by a new manual run; global Stop unaffected. Old-generation real Stop and no-frame completion isolation.

### Cycle and telemetry

- Every nonterminal phase under expiry, including a timer due at the exact deadline; no automatic recovery/restart.
- All public manual-control paths during Rest/Settling and other reserved phases; active/nonzero Rest contradiction and same-mode Active during Settling.
- Natural Finished followed by a later valid zero-current inactive report; firmware settling without rows; batch evidence reconciled before next Start; finite repeats.
- No stale-gap, contradictory, recovered, or pre-command owned attribution. Counter rebasing and energy segmentation on Continue in both controller modes. Firmware rows excluded while valid physical fields remain usable. Cycle boundary order preserved.
- TimerSync at minute/expiry boundaries, after every revocation cause, skipped intervals, and old-operation queued TimerSync.

### Required public-boundary evidence

- **Server PTY + HTTP:** actual routes (including alternate/manual Start and saved-recipe Start), real parser bytes, outbound frame capture, HTTP result, persisted run/cycle metrics, Stop/retry/Disconnect, restart. GUI capabilities alone are insufficient.
- **Native direct:** production command queue, receive/batch handling, authorization/write/completion, and PTY wire capture. Calling `LocalBackend` directly does not test native ingress provenance.
- **Browser WebUSB:** production worker with controllable actual `transferIn`/`transferOut` Promises; incomplete/never-settling output, delayed callbacks, fragmented input, old queue, freeze/resume, reconnect, full/short/stalled/rejected transfers, and cleanup. A controller fake clock alone is insufficient.
- **Remote consumers:** semantic command mapping, no physical side effects on client loss, server-side rejection even when callers ignore capabilities, no replay of commands onto a replacement session.

Actual software wire evidence is required when a change affects command admission, deduplication, scheduling, queue causality, generation, or transport completion. Electrical/hardware testing is required only when relying on new protocol/physical behavior beyond the validated contract, and must be explicitly authorized under project policy. Software simulation cannot prove that hardware obeyed Stop. Documentation-only changes do not require repeating physical battery tests.

## 18. Physical-control PR checklist

- [ ] Valid observation/integrity, receipt age, partial/queued input, causal fences, and connection replacement still obey §§3–4.
- [ ] Acquisition, revocation, terminal evidence, and no-silent-reclaim behavior remain explicit.
- [ ] Every public Start/Continue/Adjust/calibration path enforces the shared physical and cycle guards.
- [ ] Stop remains available when inactivity is unproven; deduplication/retry and blocked I/O are bounded; Disconnect can retire the channel.
- [ ] Manual/cycle reservation, Rest, Settling, interruption, and contradictory modes preserve the authority matrices.
- [ ] Owned samples, capacity, energy, elapsed time, and TimerSync cannot survive loss or bridge unknown intervals.
- [ ] Restart/reconnect, old completions, snapshots, and client disappearance cannot recreate authority.
- [ ] Server/native/WebUSB semantics remain equivalent; required public-boundary regressions and actual wire evidence are included; any new hardware assumption has authorized evidence.
- [ ] Any intended conflict with this contract is resolved by an explicit specification change, not a backend-specific bypass.
