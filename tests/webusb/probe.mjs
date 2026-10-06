// Puppeteer/Browserless entry point. Uses real worker futures and USB Promises.
export default async ({ page, context = {} }) => {
  const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
  const assert = (value, message) => { if (!value) throw new Error(message); };
  const results = [];
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  const init = await page.evaluateOnNewDocument(() => {
    let now = 1000000;
    Object.defineProperty(performance, 'now', { value: () => now });
    window.advance = ms => { now += ms; };
    const usb = window.mockUsb = { writes: [], readers: [], queued: [], held: [], hold: false, closed: 0 };
    const device = {
      productName: 'Software boundary fixture', manufacturerName: 'test', vendorId: 0x1a86, productId: 0x7523,
      configuration: { interfaces: [{ interfaceNumber: 0, alternate: { endpoints: [
        { type: 'bulk', direction: 'in', endpointNumber: 1 }, { type: 'bulk', direction: 'out', endpointNumber: 2 },
      ] } }] },
      open: async () => {}, selectConfiguration: async () => {}, claimInterface: async () => {},
      controlTransferOut: async () => ({ status: 'ok', bytesWritten: 1 }),
      close: async () => { usb.closed++; while (usb.readers.length) usb.readers.shift().reject(new Error('closed')); },
      transferIn: () => new Promise((resolve, reject) => {
        if (usb.queued.length) resolve(usb.queued.shift()); else usb.readers.push({ resolve, reject });
      }),
      transferOut: (ep, data) => {
        const bytes = Array.from(new Uint8Array(data.buffer ?? data, data.byteOffset ?? 0, data.byteLength));
        usb.writes.push({ time: now, bytes });
        if (usb.hold) { usb.hold = false; return new Promise((resolve, reject) => usb.held.push({ resolve, reject, length: bytes.length })); }
        return Promise.resolve({ status: 'ok', bytesWritten: bytes.length });
      },
    };
    Object.defineProperty(navigator, 'usb', { value: { getDevices: async () => [device] } });
    window.release = outcome => {
      const pending = usb.held.shift();
      if (outcome === 'reject') pending.reject(new Error('injected rejection'));
      else pending.resolve({ status: outcome === 'stall' ? 'stall' : 'ok', bytesWritten: outcome === 'short' ? 9 : pending.length });
    };
    window.inject = (state = 0, capacity = 0) => {
      const enc = n => [Math.floor(n / 240), n % 240];
      const p = [state, 0, [10, 11, 12, 110, 111, 112].includes(state) ? 10 : 0, ...enc(4000), ...enc(capacity), 0, 0, 0, 10, 1, 60, 0, 0, 9];
      const bytes = [250, ...p, p.reduce((a, b) => a ^ b, 0), 248];
      const result = { status: 'ok', data: new DataView(new Uint8Array(bytes).buffer) };
      if (usb.readers.length) usb.readers.shift().resolve(result); else usb.queued.push(result);
    };
  });
  const config = { mode: 'discharge_constant_current', current_ma: 100, cutoff_voltage_mv: 3000, cutoff_time_min: 0 };
  const configs = [config,
    { mode: 'discharge_constant_power', power_w: 1, cutoff_voltage_mv: 3000, cutoff_time_min: 0 },
    { mode: 'charge_constant_voltage', current_ma: 100, voltage_mv: 4200, cutoff_current_ma: 10 },
  ];
  const command = async (kind, body = {}) => {
    await page.evaluate((k, b) => wasmBindings.boundary_command(k, JSON.stringify(b)), kind, body);
    await pause(100);
  };
  const advance = ms => page.evaluate(ms => window.advance(ms), ms);
  const report = async (state = 0, capacity = 0) => { await advance(1); await page.evaluate((s, c) => inject(s, c), state, capacity); await pause(100); };
  const capture = () => page.evaluate(() => ({ events: JSON.parse(wasmBindings.boundary_events()), writes: mockUsb.writes, closed: mockUsb.closed }));
  const latest = r => r.events.filter(e => e.update || e.snapshot).at(-1)?.update ?? r.events.filter(e => e.snapshot).at(-1)?.snapshot;
  const count = (r, code) => r.writes.filter(w => w.bytes[1] === code).length;
  async function setup(observe = true) {
    await page.goto(context.url ?? 'http://host.containers.internal:18332/', { waitUntil: 'networkidle0' });
    await page.waitForFunction(() => window.wasmBindings?.boundary_start);
    await page.evaluate(() => wasmBindings.boundary_start());
    await command('connect');
    if (observe) await report();
    await capture();
  }

  let r;
  if (context.section !== 'modes') {
  await setup(false);
  await command('stop');
  r = await capture();
  assert(count(r, 2) === 1 && !latest(r).device.activity_known && latest(r).current_run.id === null, 'unknown Stop must write without creating owned work');
  await advance(10000); await command('stop');
  r = await capture(); assert(count(r, 2) === 2, 'initial expiry must permit real Stop retry');
  await advance(30000); await command('stop');
  r = await capture(); assert(count(r, 2) === 3, 'consumed expiry must not latch unknown Stop');
  await command('disconnect'); r = await capture(); assert(count(r, 2) === 4 && count(r, 6) === 1, 'safe disconnect must write Stop then Disconnect');
  results.push('unknown connection safety Stop / expiry / retry / disconnect');

  for (const outcome of ['ok', 'reject', 'short', 'stall']) {
    for (const states of [[0], [10], [100], [110], [0, 10, 100, 0]]) {
      await setup();
      // Retain a real stopped configuration in the Idle/full-success case, so
      // Resume rejection cannot pass merely because configuration is absent.
      if (outcome === 'ok' && states.length === 1 && states[0] === 0) {
        await command('start', { config }); await report(10, 1);
        await command('stop'); await report();
      }
      const prior = await capture();
      const startsBefore = count(prior, 1), stopsBefore = count(prior, 2);
      await page.evaluate(() => { mockUsb.hold = true; });
      await command('start', { config });
      await page.waitForFunction(() => mockUsb.held.length === 1);
      await advance(100);
      for (const state of states) { await page.evaluate(s => inject(s, 30), state); await pause(50); }
      // Exact receipt age == timeout. The transfer completes after that boundary.
      await advance(10000); await page.evaluate(o => release(o), outcome); await pause(250);
      r = await capture();
      assert(!latest(r).device.activity_known && latest(r).test.state === 'recovered_uncertain', `${outcome}/${states}: old report restored authority`);
      assert(!r.events.some(e => e.sample || e.cycle_sample), 'old reports cannot sample');
      await command('start', { config }); await command('resume', config);
      await command('api', { command: 'adjust', payload: config });
      await command('api', { command: 'calibration', payload: { operation: 'voltage_low', value: 4000 } });
      r = await capture(); assert(count(r, 1) === startsBefore + 1 && count(r, 8) === 0 && count(r, 7) === 0 && count(r, 4) === 0, 'stale observations authorized another physical command');
      if (outcome === 'ok') {
        await command('stop'); r = await capture(); assert(count(r, 2) === stopsBefore + 1, 'explicit Stop must survive discarded input');
      }
      await command('connect'); await report(); r = await capture();
      assert(latest(r).device.activity_known, 'actual fresh report after reconnect must restore observation');
      assert(!r.events.some(e => e.sample), 'reconnect must not reclaim metrics');
      await command('shutdown');
    }
    results.push(`delayed ${outcome}: Idle/Active/firmware/multiple queue, equality, safety commands, reconnect`);
  }

  for (const state of [0, 10, 100, 110]) {
    await setup();
    await page.evaluate(() => { mockUsb.hold = true; });
    await command('start', { config });
    await page.waitForFunction(() => mockUsb.held.length === 1);
    await report(state, 30);
    // Most cases tie receipt and completion on the controlled coarse clock.
    // Firmware-inactive also exercises a strictly earlier (but still fresh) receipt.
    if (state === 100) await advance(100);
    await page.evaluate(() => release('ok')); await pause(200);
    r = await capture();
    assert(latest(r).test.state === 'starting' && !latest(r).device.activity_known, 'queued pre-completion report acknowledged later Start');
    await report(10, 1); r = await capture(); assert(latest(r).test.state === 'running', 'new post-command observation must acknowledge Start');
    await command('stop'); await command('stop'); r = await capture(); assert(count(r, 2) === 1, 'fresh pending Stop must deduplicate');
    await report(10, 2); await command('stop'); r = await capture(); assert(count(r, 2) === 2, 'fresh Active must allow Stop retry');
    await command('shutdown');
  }
  results.push('fresh but pre-command queued observations never acknowledge Start; genuine recovery and Stop dedup');

  for (let wanted = 0; wanted < 3; wanted++) {
    for (let previous = 0; previous < 3; previous++) {
      if (wanted === previous) continue;
      for (const state of [previous, 20 + previous, 100 + previous]) {
        await setup(); await report(previous);
        const cycle = state >= 20;
        if (cycle) await command('cycle', { recipe: { steps: [{ type: 'device', config: configs[wanted], completion: 'hardware' }], repeat_count: 1 } });
        else await command('start', { config: configs[wanted] });
        await report(state, 30); r = await capture();
        assert(latest(r).test.state === 'starting' && !r.events.some(e => e.sample), 'prior-mode inactive report revoked pending Start or owned its metrics');
        if (cycle) assert(latest(r).cycle.state === 'starting_step', 'prior-mode inactive report interrupted pending cycle Start');
        await report(10 + wanted, 1); r = await capture(); assert(latest(r).test.state === 'running', 'expected-mode Active must confirm pending Start');
        await command('shutdown');
      }
    }
  }
  results.push('CC/CP/CV pending Start tolerates prior-mode Idle/Finished/firmware-inactive until expected Active, without owned metrics');

  for (const intent of [
    { command: 'adjust', payload: config },
    { command: 'calibration', payload: { operation: 'voltage_low', value: 4000 } },
  ]) {
    for (const state of [11, 111]) {
      await setup(); await command('start', { config }); await report(10, 1); await capture();
      await page.evaluate(() => { mockUsb.hold = true; });
      await command('api', intent); await page.waitForFunction(() => mockUsb.held.length === 1);
      await report(state, 30); await advance(100); await page.evaluate(() => release('ok')); await pause(200);
      r = await capture();
      assert(latest(r).test.state === 'recovered_uncertain' && latest(r).device.active, 'fresh queued contradiction during Adjust/calibration retained ownership');
      assert(!r.events.some(e => e.sample), 'queued contradiction became an owned sample');
      await report(10, 100); r = await capture(); assert(latest(r).test.state === 'recovered_uncertain', 'return to expected mode reclaimed queued contradictory work');
      await command('stop'); r = await capture(); assert(count(r, 2) === 1, 'queued contradiction lost safety Stop');
      await command('shutdown');
    }
  }
  results.push('fresh queued ordinary/firmware contradiction during Adjust/calibration revokes ownership without acknowledging new lifecycle intent');

  await setup();
  await command('cycle', { recipe: { steps: [{ type: 'device', config, completion: 'hardware' }, { type: 'device', config, completion: 'hardware' }], repeat_count: 1 } });
  await report(10, 1);
  // Keep reports genuinely fresh until TimerSync is due, then block its Promise.
  for (let i = 0; i < 12; i++) { await advance(5000); await report(10, i + 2); }
  await page.evaluate(() => { mockUsb.hold = true; });
  await advance(1000); await pause(250);
  // If the timer already wrote during the last report's tick, start a new minute.
  if (!(await page.evaluate(() => mockUsb.held.length))) {
    for (let i = 0; i < 12; i++) { await advance(5000); await report(10, i + 20); if (await page.evaluate(() => mockUsb.held.length)) break; }
  }
  assert(await page.evaluate(() => mockUsb.held.length === 1), 'TimerSync fixture did not reach held transfer');
  await report(20, 30); await report(0, 30); await advance(10001);
  await page.evaluate(() => release('ok')); await pause(250); r = await capture();
  assert(latest(r).cycle.state === 'interrupted' && !latest(r).device.activity_known && count(r, 1) === 1, 'stale Finished/settling zero advanced cycle');
  const timers = count(r, 10); await advance(60000); await pause(200); r = await capture(); assert(count(r, 10) === timers, 'uncertain cycle emitted TimerSync');
  await report(); r = await capture(); assert(latest(r).cycle.state === 'interrupted' && count(r, 1) === 1, 'fresh recovery resumed interrupted cycle');
  await command('shutdown'); results.push('queued Finished/settling-zero cannot advance; TimerSync suppression and observation-only recovery');

  await setup(); await command('cycle', { recipe: { steps: [{ type: 'rest', duration_seconds: 12 }, { type: 'device', config, completion: 'hardware' }], repeat_count: 1 } });
  const cdp = await page.createCDPSession(); await cdp.send('Page.setWebLifecycleState', { state: 'frozen' });
  await cdp.send('Page.setWebLifecycleState', { state: 'active' }); await advance(12000); await pause(250);
  r = await capture(); assert(latest(r).cycle.state === 'interrupted' && count(r, 1) === 0, 'tab resumption must expire before Rest progression');
  await report(); r = await capture(); assert(latest(r).cycle.state === 'interrupted', 'tab recovery resumed cycle');
  await command('shutdown'); await cdp.detach(); results.push('tab suspension/resumption expiry before autonomous Rest Start');
  }

  if (context.section !== 'queue') {
  for (let owned = 0; owned < 3; owned++) {
    for (let observed = 0; observed < 3; observed++) {
      if (owned === observed) continue;
      for (const firmware of [false, true]) {
        for (const cycle of [false, true]) {
          await setup();
          if (cycle) await command('cycle', { recipe: { steps: [{ type: 'device', config: configs[owned], completion: 'hardware' }], repeat_count: 1 } });
          else await command('start', { config: configs[owned] });
          if (firmware) await report(10 + owned, 1);
          const before = await capture();
          await report((firmware ? 110 : 10) + observed, 30); r = await capture();
          assert(latest(r).test.state === 'recovered_uncertain' && latest(r).device.active, 'mode mismatch retained ownership or hid live activity');
          assert(!r.events.some(e => e.sample), 'contradictory report became an owned sample');
          if (cycle) assert(latest(r).cycle.state === 'interrupted', 'mode contradiction did not interrupt cycle');
          const capacity = latest(r).test.capacity_mah;
          for (let i = 0; i < 13; i++) { await advance(5000); await report(10 + observed, 31 + i); }
          await report(10 + owned, 100); r = await capture();
          assert(latest(r).test.state === 'recovered_uncertain' && latest(r).test.capacity_mah === capacity && count(r, 10) === count(before, 10), 'fresh reports reclaimed metrics/TimerSync');
          assert(!r.events.some(e => e.sample), 'sustained contradiction appended owned samples');
          await command('stop'); r = await capture(); assert(count(r, 2) === 1, 'contradictory operation lost safety Stop');
          await report(owned); r = await capture(); assert(latest(r).test.state === 'stopped', 'explicit Stop followed by fresh inactive report did not recover');
          await command('shutdown');
        }
      }
    }
  }
  results.push('CC/CP/CV contradictions: first Active/running, ordinary/firmware, manual/cycle, sustained beyond TimerSync, Stop/recovery');
  }
  await page.removeScriptToEvaluateOnNewDocument(init.identifier);
  assert(errors.length === 0, `browser exceptions: ${errors}`);
  return { data: { passed: results }, type: 'application/json' };
};
