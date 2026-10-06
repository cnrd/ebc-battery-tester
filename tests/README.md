# Physical-authority boundary regressions

All fixtures are software-only; they must not open a real tester. CI runs them on
every PR alongside the ordinary Rust tests and native container smoke jobs.

## Production server

```sh
cargo build --locked --no-default-features --features server --bin ebc-server
python tests/physical_authority.py target/debug/ebc-server <new-scratch-directory>
```

This starts the actual server against Linux PTYs and uses real HTTP commands and
parsed protocol reports. It checks unknown/stale safety Stop, all CC/CP/CV mode
contradictions (manual/cycle, first Active/Running, ordinary/firmware), and fresh
contradictory observations lasting longer than a TimerSync interval. Wire bytes,
history, metrics, conservative state, and explicit recovery are asserted. Artifacts
are written only to the supplied new directory; failures preserve them.

Native direct Rust tests in `src/usb_native.rs` also run the production
authorization/write/completion path against software PTY pairs. Shared local tests
exercise receipt-age equality, parsed firmware sampling, and connection generations.

## Browser WebUSB worker

```sh
python tests/webusb/prepare.py <new-scratch-source>
# From that scratch source (uses the repository's .cargo configuration):
EBC_WASM_DEFAULT_TRANSPORT=remote trunk build --locked --dist <scratch-dist>
# From the source repository:
npm ci --prefix tests/webusb
CHROME_PATH=<chromium-executable> node tests/webusb/run.mjs <scratch-dist>
```

The preparation script copies source and appends a visibility-only adapter to the
scratch copy. Production parser, worker, controller, cycle and USB transfer checks
remain unchanged. No adapter is shipped in normal builds. The JavaScript USB fixture
controls the actual `transferIn`/`transferOut` Promises, including full/short/stalled
success and rejection. A controlled monotonic browser clock tests exact age equality
and long schedules without wall-clock sleeps. Coverage includes queued ordinary and
firmware reports, multiple frames, pre-command acknowledgements, settling-zero,
generation replacement, tab resumption, Start/Resume/Adjust/calibration rejection,
Stop retries/deduplication, and sustained mode contradictions. `probe.mjs` can also
run through Browserless using its `{page, context}` entry point (sections `queue`
and `modes`). The auditor's real-time 11.2-second delay is rerun separately during
release remediation; simulated clocks are not hardware or real tab-lifecycle proof.
