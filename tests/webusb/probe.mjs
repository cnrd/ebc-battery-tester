// Puppeteer/Browserless entry point. Uses real worker futures and USB Promises.
export default async ({ page, context = {} }) => {
  const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
  const assert = (value, message) => { if (!value) throw new Error(message); };
  const results = [];
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  const init = await page.evaluateOnNewDocument(() => {
     let now = 1000000;
     let wall = Date.now();
     Object.defineProperty(performance, 'now', { value: () => now });
     Object.defineProperty(Date, 'now', { value: () => wall });
     window.advance = ms => { now += ms; wall += ms; };
     window.advanceWall = ms => { wall += ms; };
    const usb = window.mockUsb = { writes: [], readers: [], queued: [], held: [], hold: false, closed: 0, holdClose: false, heldClose: [] };
    const device = {
      productName: 'Software boundary fixture', manufacturerName: 'test', vendorId: 0x1a86, productId: 0x7523,
      configuration: { interfaces: [{ interfaceNumber: 0, alternate: { endpoints: [
        { type: 'bulk', direction: 'in', endpointNumber: 1 }, { type: 'bulk', direction: 'out', endpointNumber: 2 },
      ] } }] },
       open: async () => { if (usb.holdOpen) { usb.holdOpen = false; await new Promise(resolve => { usb.releaseOpen = resolve; }); } },
       selectConfiguration: async () => { if (usb.failConfiguration) throw new Error('injected configuration failure'); }, claimInterface: async () => {},
      controlTransferOut: async () => ({ status: 'ok', bytesWritten: 1 }),
      close: async () => { usb.closed++; while (usb.readers.length) usb.readers.shift().reject(new Error('closed')); if (usb.holdClose) { usb.holdClose = false; return new Promise(resolve => usb.heldClose.push(resolve)); } },
      transferIn: () => new Promise((resolve, reject) => {
        if (usb.queued.length) resolve(usb.queued.shift()); else usb.readers.push({ resolve, reject });
      }),
      transferOut: (ep, data) => {
        const bytes = Array.from(new Uint8Array(data.buffer ?? data, data.byteOffset ?? 0, data.byteLength));
        usb.writes.push({ time: now, bytes });
         if (usb.hold || usb.holdCode === bytes[1]) { usb.hold = false; usb.holdCode = null; return new Promise((resolve, reject) => usb.held.push({ resolve, reject, length: bytes.length })); }
        return Promise.resolve({ status: 'ok', bytesWritten: bytes.length });
      },
    };
    Object.defineProperty(navigator, 'usb', { value: { getDevices: async () => [device] } });
    window.release = outcome => {
      const pending = usb.held.shift();
      if (outcome === 'reject') pending.reject(new Error('injected rejection'));
      else pending.resolve({ status: outcome === 'stall' ? 'stall' : 'ok', bytesWritten: outcome === 'short' ? 9 : pending.length });
    };
     window.reportBytes = (state = 0, capacity = 0, current) => {
      const enc = n => [Math.floor(n / 240), n % 240];
       const p = [state, ...enc(current ?? ([10, 11, 12, 110, 111, 112].includes(state) ? 10 : 0)), ...enc(4000), ...enc(capacity), 0, 0, 0, 10, 1, 60, 0, 0, 9];
      return [250, ...p, p.reduce((a, b) => a ^ b, 0), 248];
    };
    window.injectBytes = bytes => {
      const result = { status: 'ok', data: new DataView(new Uint8Array(bytes).buffer) };
      if (usb.readers.length) usb.readers.shift().resolve(result); else usb.queued.push(result);
    };
     window.inject = (state = 0, capacity = 0, current) => injectBytes(reportBytes(state, capacity, current));
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
  // A read issued before a lifecycle write is conservatively pre-fence even
  // when its callback comes later. Send a separate subsequent observation; do
  // not retimestamp or reinject the held transfer as post-command evidence.
  const report = async (state = 0, capacity = 0) => {
    for (let i = 0; i < 2; i++) { await advance(1); await page.evaluate((s, c) => inject(s, c), state, capacity); await pause(100); }
  };
   let currentState;
   const capture = async () => {
     const r = await page.evaluate(() => ({ events: JSON.parse(wasmBindings.boundary_events()), writes: mockUsb.writes, closed: mockUsb.closed }));
     const state = r.events.filter(e => e.update || e.snapshot).at(-1);
     if (state) currentState = state.update ?? state.snapshot;
     return {...r, state: currentState};
   };
   const latest = r => r.state;
  const count = (r, code) => r.writes.filter(w => w.bytes[1] === code).length;
  async function setup(observe = true) {
     currentState = undefined;
    await page.goto(context.url ?? 'http://host.containers.internal:18332/', { waitUntil: 'networkidle0' });
    await page.waitForFunction(() => window.wasmBindings?.boundary_start);
    await page.evaluate(() => wasmBindings.boundary_start());
    await command('connect');
    if (observe) await report();
    await capture();
  }

  let r;
   if (context.section !== 'modes' && context.section !== 'conformance') {
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
       // Full success delivered after the attempt deadline is retired too. Stop
       // remains a finite error on that unusable channel, not a fabricated write.
       await command('stop'); r = await capture(); assert(count(r, 2) === stopsBefore && r.closed > 0, 'retired timeout channel must not issue a fake Stop');
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

   if (context.section !== 'queue' && context.section !== 'conformance') {
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

   if (context.section === 'conformance') {
     await setup(false); await command('disconnect'); await capture();
     await page.evaluate(() => { mockUsb.holdOpen = true; }); await command('connect');
     await advance(10000); await pause(400); r = await capture();
     const closedBeforeRetry = r.closed;
     assert(latest(r).connection === 'error' && closedBeforeRetry >= 2, 'timed-out open skipped bounded resource retirement');
     await command('connect'); r = await capture(); assert(latest(r).connection === 'error', 'unresolved old open allowed replacement handle use');
     await page.evaluate(() => mockUsb.releaseOpen()); await pause(200); r = await capture();
     assert(!r.events.some(e => e.update || e.snapshot), 'late old open mutated replacement authority');
     await command('shutdown'); results.push('never-resolving setup open attempts close and quarantines handle against a late open');

     await setup(false); await command('disconnect');
     await page.evaluate(() => { mockUsb.failConfiguration = true; }); await command('connect'); r = await capture();
     assert(latest(r).connection === 'error' && r.closed >= 2, 'failed setup after successful open leaked resource without close');
     await command('shutdown'); results.push('configuration failure after successful open still attempts bounded local close');

     await setup(); await command('start', {config}); await report(10, 1); await capture();
     // Model a browser whose runtime monotonic clock excludes host sleep. UTC
     // discontinuity may revoke authority, never create or refresh an observation.
     await page.evaluate(() => advanceWall(11000));
     await command('api', {command:'calibration',payload:{operation:'voltage_low',value:4000}});
     r = await capture();
     assert(count(r, 4) === 0 && latest(r).connection === 'error' && r.closed > 0,
       'sleep excluded by performance clock retained ordinary physical authority');
     await command('shutdown'); results.push('suspend-excluding host clock revokes before ordinary action and quarantines input');

     await setup(false); await report(10, 50); await capture();
     for (const operation of ['voltage_low', 'confirm']) {
       await command('api', {command:'calibration',payload:{operation,...(operation === 'confirm' ? {} : {value:4000})}});
     }
     r = await capture(); assert(count(r, 4) === 0 && latest(r).test.state === 'recovered_uncertain', 'unowned Active enabled calibration or Confirm');
     await command('stop'); r = await capture(); assert(count(r, 2) === 1, 'unowned Active lost safety Stop');
     await command('shutdown'); results.push('unowned Active rejects voltage calibration and Confirm through worker command queue');

     const calibration = (operation, value) => command('api', {command:'calibration',payload:{operation,...(value === undefined ? {} : {value})}});
     await setup(); await calibration('confirm'); r = await capture(); assert(count(r, 4) === 0, 'direct Confirm bypassed four-reference staging');
     await calibration('voltage_low', 1000); await calibration('voltage_high', 4000);
     await command('start', {config}); await report(10, 1);
     await calibration('current_low', 100); await calibration('current_high', 1000);
     await command('stop'); await report(); await calibration('confirm'); r = await capture();
     assert(count(r, 4) === 5 && latest(r).capabilities.confirm_calibration === false, 'normal Start/Stop did not preserve and consume legitimate staging');
     await command('connect'); await report(); await calibration('confirm'); r = await capture(); assert(count(r, 4) === 5, 'reconnect retained calibration staging');
     await command('shutdown'); results.push('shared four-reference staging survives authorized Start/Stop, Confirm consumes, reconnect clears');

     for (const phase of ['rest', 'settling']) {
       await setup();
       const steps = phase === 'rest' ? [{type:'rest',duration_seconds:30},{type:'device',config,completion:'hardware'}]
         : [{type:'device',config,completion:'hardware'},{type:'device',config,completion:'hardware'}];
       await command('cycle', {recipe:{steps,repeat_count:1}});
       if (phase === 'settling') { await report(10, 1); await advance(1); await page.evaluate(() => inject(20, 1)); await pause(150); }
       const before = await capture();
       await command('start', {config}); await command('api', {command:'start',payload:config});
       await command('resume', config); await command('api', {command:'adjust',payload:config});
       for (const [operation,value] of [['voltage_low',1000],['voltage_high',4000],['current_low',100],['current_high',1000],['confirm',undefined]]) await calibration(operation,value);
       r = await capture();
       assert(count(r,1) === count(before,1) && count(r,8) === 0 && count(r,7) === 0 && count(r,4) === 0,
         `manual command bypassed ${phase} orchestration reservation`);
       assert(latest(r).cycle.state === (phase === 'rest' ? 'resting' : 'settling'), 'rejected manual intent changed cycle');
       await command('shutdown');
     }
     results.push('all manual Start/Continue/Adjust/calibration entry points deny reserved Rest and Settling without wire changes');

     for (const state of [0, 10, 100, 110]) {
       await setup(); await command('cycle', {recipe:{steps:[{type:'rest',duration_seconds:30},{type:'device',config,completion:'hardware'}],repeat_count:1}});
       await advance(1); await page.evaluate(s => inject(s, 50, 10), state); await pause(200); r = await capture();
       assert(latest(r).cycle.state === 'interrupted' && count(r,1) === 0, 'Rest tolerated active/nonzero ordinary/firmware evidence');
       await report(); await advance(30000); await pause(200); r = await capture();
       assert(latest(r).cycle.state === 'interrupted' && count(r,1) === 0, 'Rest silently resumed after recovered zero current');
       await command('shutdown');
     }
     results.push('Rest Active and inactive/nonzero contradictions, ordinary/firmware, remain latched across zero recovery and timer expiry');

     await setup(); await command('start',{config}); await report(10,10);
     await advance(10000); await pause(200); await command('connect'); await report(0,50);
     await command('resume',config); await report(10,51); r = await capture();
     assert(count(r,8) === 1 && latest(r).test.capacity_mah === 11 && Math.abs(latest(r).test.energy_wh - .044) < 1e-9,
       'Continue imported unowned raw capacity/estimated energy after gap/reconnect');
     assert(r.events.some(e => e.sample?.capacity_mah === 11) && !r.events.some(e => e.sample?.capacity_mah === 51), 'direct owned sample used raw counter history');
     await command('shutdown'); results.push('direct Continue after gap/reconnect excludes unowned +40 mAh from metrics and delivered samples');

     await setup(); await command('cycle',{recipe:{steps:[{type:'device',config,completion:'hardware'},{type:'device',config,completion:'hardware'}],repeat_count:1}});
     await report(10,10); await report(100,11); await capture();
     await advance(1); await page.evaluate(() => inject(20,12)); await pause(150); r = await capture();
     assert(latest(r).cycle.state === 'settling' && count(r,1) === 1 && latest(r).test.capacity_mah === 11,
       'closing ordinary Finished failed classification or renewed final-counter attribution');
     await advance(1); await page.evaluate(() => inject(100,12)); await pause(200); r = await capture();
     assert(count(r,1) === 2 && latest(r).test.state === 'starting' && !r.events.some(e => e.cycle_sample || e.sample),
       'later firmware zero did not satisfy Settling or fabricated telemetry');
     await command('shutdown'); results.push('firmware closing then ordinary Finished classifies; later firmware zero advances without firmware rows');

     await setup(); await page.evaluate(() => { mockUsb.hold = true; }); await command('start',{config});
     await command('stop'); await command('disconnect');
     const frozen = await page.createCDPSession(); await frozen.send('Page.setWebLifecycleState',{state:'frozen'});
     await frozen.send('Page.setWebLifecycleState',{state:'active'});
     await page.evaluate(() => { advanceWall(11000); release('ok'); }); await pause(400); r = await capture();
     assert(latest(r).connection === 'error' && r.closed > 0 && count(r,2) === 0 && !latest(r).device.activity_known,
       'freeze/resume delivered late ordinary completion before timeout/retirement');
     await command('shutdown'); await frozen.detach(); results.push('actual tab freeze/resume with held output and suspend-excluding clock retires before late completion');
     for (const boundary of ['start', 'stop', 'expiry', 'replacement']) {
       await setup();
       if (boundary === 'stop') { await command('start', {config}); await report(10, 1); }
       await advance(1);
       await page.evaluate(() => { window.fragment = reportBytes(10, 50); injectBytes(fragment.slice(0, 18)); });
       await pause(100);
       if (boundary === 'start') await command('start', {config});
       if (boundary === 'stop') await command('stop');
       if (boundary === 'replacement') await command('connect');
       if (boundary === 'expiry') await advance(10000);
       await advance(1); await page.evaluate(() => injectBytes(fragment.slice(18))); await pause(300);
       r = await capture();
       assert(latest(r).test.state !== 'running', `fragment across ${boundary} acquired/revived ownership`);
       assert(!r.events.some(e => e.sample && e.sample.capacity_mah === 50), 'old prefix attributed capacity');
       await command('shutdown');
     }
     results.push('fragment oldest-byte age and Start/Stop/replacement fences through actual transferIn');

     await setup();
     await command('cycle', {recipe: {steps: [{type:'device',config,completion:'hardware'}, {type:'device',config,completion:'hardware'}],repeat_count:1}});
     await report(10, 1); await capture();
     await advance(1); await page.evaluate(() => inject(20, 1)); await pause(150);
     r = await capture(); assert(latest(r).cycle.state === 'settling', 'single Finished must enter Settling');
     await advance(1); await page.evaluate(() => injectBytes([...reportBytes(0, 1), ...reportBytes(10, 30), ...reportBytes(0, 31)])); await pause(250);
     r = await capture();
     assert(latest(r).cycle.state === 'interrupted' && count(r, 1) === 1, 'available Idle/Active/Idle prefix emitted next child Start or hid contradiction');
     await command('shutdown'); results.push('available prefix barrier retains intermediate Settling contradiction and emits no next Start');

     for (const cycle of [false, true]) {
       await setup();
       if (cycle) await command('cycle', {recipe:{steps:[{type:'device',config,completion:'hardware'}],repeat_count:1}});
       else await command('start', {config});
       const started = (await capture()).writes.filter(w => w.bytes[1] === 1).at(-1).time;
       for (let i = 0; i < 9; i++) { await advance(990); await report(0, i); }
       await page.evaluate(started => { advance(started + 10000 - performance.now()); inject(10, 50); }, started);
       await pause(300); r = await capture();
       assert(latest(r).test.state === 'recovered_uncertain' && latest(r).test.capacity_mah === null, 'inactive telemetry extended acquisition or equality acquired');
       if (cycle) assert(latest(r).cycle.state === 'interrupted', 'child acquisition deadline did not interrupt cycle');
       await command('shutdown');
     }
     results.push('manual and child acquisition exact equality despite healthy inactive transferIn');

     for (const cycle of [false, true]) {
       await setup();
       if (cycle) await command('cycle', {recipe:{steps:[{type:'device',config,completion:'hardware'}],repeat_count:1}});
       else await command('start', {config});
       await report(10, 10); await report(100, 11); await capture();
       await report(10, 50); r = await capture();
       assert(latest(r).test.state === 'recovered_uncertain' && latest(r).test.capacity_mah === 11 && !r.events.some(e => e.sample), 'firmware-inactive retained dormant ownership');
       if (cycle) assert(latest(r).cycle.state === 'interrupted', 'firmware-inactive then Active kept cycle');
       await command('shutdown');
     }
     results.push('firmware-inactive closes manual/child control without metric restart');

     for (const intent of ['start', 'stop']) {
       await setup();
       if (intent === 'stop') { await command('start',{config}); await report(10, 1); }
       await page.evaluate(() => { mockUsb.hold = true; });
       await command(intent, intent === 'start' ? {config} : {});
       await command('stop'); await command('disconnect');
       await advance(10000); await pause(600); r = await capture();
       assert(r.closed > 0 && latest(r).connection === 'error', 'hung output trapped Disconnect/retirement');
       assert(count(r, 1) === 1 && count(r, 2) === (intent === 'stop' ? 1 : 0), 'hung ordinary/Stop output raced another writer');
       await command('connect'); await report(); await command('start',{config}); await report(10, 2); await capture();
       await page.evaluate(() => release('ok')); await pause(250); r = await capture();
       assert(!r.events.some(e => e.snapshot || e.update || e.sample), 'late retired successful Promise mutated replacement operation');
       await command('shutdown');
     }
     results.push('never-resolving ordinary/Stop output; queued Stop/Disconnect; exact timeout; late success has zero replacement effects');

     await setup(); await page.evaluate(() => { mockUsb.holdCode = 6; });
     await command('disconnect'); await page.waitForFunction(() => mockUsb.held.length === 1);
     await page.evaluate(() => release('reject')); await pause(250); r = await capture();
     assert(r.closed === 1 && latest(r).connection === 'error', 'failed protocol Disconnect skipped local close');
     results.push('rejected protocol Disconnect still attempts local resource close');

     await setup(false); await page.evaluate(() => { mockUsb.holdClose = true; });
     await command('disconnect'); await advance(10000); await pause(350); r = await capture();
     assert(r.closed === 1 && latest(r).connection === 'error', 'never-resolving close trapped finite retirement');
     await command('connect'); r = await capture(); assert(latest(r).connection === 'error', 'unretired handle reopened under a late close');
     await page.evaluate(() => mockUsb.heldClose.shift()()); await pause(250); r = await capture();
     assert(!r.events.some(e => e.snapshot || e.update), 'late close resurrected retired connection');
     results.push('bounded close timeout and quarantine of unresolved handle; late close has zero authority');

     await setup(); await command('start',{config}); await report(10, 1); await capture();
     await page.evaluate(() => { inject(10, 50); advance(10000); }); await pause(300); r = await capture();
     assert(latest(r).connection === 'error' && !r.events.some(e => e.sample), 'delayed input continuation manufactured freshness');
     await command('shutdown'); results.push('already-resolved input Promise continuation delayed across receipt boundary fails closed');
   }
  await page.removeScriptToEvaluateOnNewDocument(init.identifier);
  assert(errors.length === 0, `browser exceptions: ${errors}`);
  return { data: { passed: results }, type: 'application/json' };
};
