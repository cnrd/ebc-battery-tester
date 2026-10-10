"""Production server/HTTP/PTY regressions. Never opens a real serial device.

Usage: python tests/physical_authority.py <ebc-server> <new scratch directory>
"""
import concurrent.futures
import csv
import functools
import json
import os
import pathlib
import select
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

CONFIGS = [
    dict(mode="discharge_constant_current", current_ma=100, cutoff_voltage_mv=3000, cutoff_time_min=0),
    dict(mode="discharge_constant_power", power_w=1, cutoff_voltage_mv=3000, cutoff_time_min=0),
    dict(mode="charge_constant_voltage", current_ma=100, voltage_mv=4200, cutoff_current_ma=10),
]
MODES = ["DischargeConstantCurrent", "DischargeConstantPower", "ChargeConstantVoltage"]
STOP = bytes.fromhex("fa0200000000000002f8")
DISCONNECT = bytes.fromhex("fa0600000000000006f8")


def frame(state, capacity=0, current=None):
    active = state in (10, 11, 12, 110, 111, 112)
    payload = [state, *divmod((10 if active else 0) if current is None else current, 240), *divmod(4000, 240), *divmod(capacity, 240), 0, 0, 0, 10, 1, 60, 0, 0, 9]
    return bytes([250, *payload, functools.reduce(int.__xor__, payload), 248])


class Rig:
    def __init__(self, binary, directory):
        self.directory = directory
        directory.mkdir(parents=True)
        self.master, self.slave = os.openpty()
        self.wire = bytearray()
        self.events = []
        self.alive = True
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}/api"
        env = dict(os.environ, EBC_SERIAL_PORT=os.ttyname(self.slave), EBC_HTTP_ADDR=f"127.0.0.1:{port}",
                   EBC_DATA_DIR=str(directory / "data"), EBC_STATIC_DIR=str(directory), EBC_MDNS="false", EBC_MOCK="false")
        self.binary, self.env = binary, env
        self.log = (directory / "server.log").open("wb")
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.process = subprocess.Popen([binary], env=env, stdout=self.log, stderr=self.log)
        try:
            for _ in range(100):
                try:
                    self.status()
                    time.sleep(.1)
                    return
                except (OSError, urllib.error.URLError):
                    time.sleep(.05)
            raise AssertionError("server did not start")
        except BaseException:
            self.close()
            raise

    def read(self):
        while self.alive:
            if select.select([self.master], [], [], .05)[0]:
                try:
                    self.wire.extend(os.read(self.master, 4096))
                except OSError:
                    pass

    def request(self, path, body=None, post=False):
        data = json.dumps(body).encode() if body is not None else (b"" if post else None)
        request = urllib.request.Request(self.url + path, data=data,
                                        headers={"Content-Type": "application/json", "X-EBC-Command": "1"})
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                code, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            code, raw = error.code, error.read()
        value = json.loads(raw)
        self.events.append(dict(path=path, code=code, value=value))
        return code, value

    def post(self, path, body=None):
        command = {"/test/stop": 2, "/disconnect": 6}.get(path)
        if path == "/test/start":
            command = 1 + 16 * next(i for i, config in enumerate(CONFIGS) if config["mode"] == body["config"]["mode"])
        elif path == "/cycle/start" and body["recipe"]["steps"][0]["type"] == "device":
            config = body["recipe"]["steps"][0]["config"]
            command = 1 + 16 * next(i for i, value in enumerate(CONFIGS) if value["mode"] == config["mode"])
        before = self.count(command) if command is not None else None
        code, value = self.request(path, body, True)
        assert code == 200, (path, code, value)
        if command is not None:
            self.wait(lambda: self.count(command) > before, f"wire command {command:#x} after {path}")
        return value

    def status(self):
        return self.request("/status")[1]

    def observe(self, state=0, capacity=0):
        os.write(self.master, frame(state, capacity))
        active = state in (10, 11, 12, 110, 111, 112)

        def observed():
            device = self.status()["device"]
            return (device["activity_known"] and device["active"] == active
                    and device["mode"] == MODES[state % 10] and device["capacity_mah"] == capacity)

        # HTTP commands and serial reads share an actor, not an 80ms fixture
        # timer. Wait for the actual observation; never inject extra recovery data.
        self.wait(observed, f"physical report state={state} capacity={capacity}")

    def wait(self, predicate, description):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(.025)
        raise AssertionError((description, self.status(), bytes(self.wire).hex()))

    def count(self, command):
        return sum(self.wire[i + 1] == command for i in range(0, len(self.wire) - 9, 10))

    def restart(self):
        self.process.terminate()
        self.process.wait(timeout=5)
        before = {command: self.count(command) for command in (1, 8, 17, 24, 33, 40)}
        self.process = subprocess.Popen([self.binary], env=self.env, stdout=self.log, stderr=self.log)
        for _ in range(100):
            try:
                self.status()
                break
            except OSError:
                time.sleep(.05)
        else:
            raise AssertionError("server did not restart")
        assert {command: self.count(command) for command in before} == before

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.alive = False
        self.reader.join()
        os.close(self.master)
        os.close(self.slave)
        self.log.close()
        (self.directory / "wire.bin").write_bytes(self.wire)
        (self.directory / "http.json").write_text(json.dumps(self.events, indent=2))


def safety_stop(rig, state):
    if state != "unknown":
        rig.observe()
        if state != "idle":
            rig.post("/test/start", dict(config=CONFIGS[0]))
            rig.observe(10, 1)
            if state == "stopped":
                rig.post("/test/stop")
            rig.observe(20 if state == "completed" else 0, 1)
            assert rig.status()["test"]["state"] == state, rig.status()
        # Fresh inactive is proven, so no safety Stop is needed until it expires.
        assert not rig.status()["capabilities"]["stop"]
    else:
        rig.post("/test/stop")
        assert rig.count(2) == 1 and not rig.status()["device"]["activity_known"]
        assert rig.status()["current_run"]["id"] is None
    time.sleep(10.3)
    before = rig.count(2)
    rig.post("/test/stop")
    assert rig.count(2) == before + 1 and bytes(rig.wire[-10:]) == STOP
    rig.post("/test/stop")  # Unknown Stop is not a fresh deduplicated acknowledgement.
    assert rig.count(2) == before + 2
    status = rig.status()
    assert not status["device"]["activity_known"] and status["test"]["state"] != "running"
    rig.post("/disconnect")
    assert bytes(rig.wire[-20:]) == STOP + DISCONNECT


def contradiction(rig, owned, observed, firmware, cycle, first, sustained=False):
    rig.observe(owned)
    if cycle:
        recipe = dict(steps=[dict(type="device", config=CONFIGS[owned], completion="hardware")], repeat_count=1)
        rig.post("/cycle/start", dict(recipe=recipe))
    else:
        rig.post("/test/start", dict(config=CONFIGS[owned]))
    if not first:
        rig.observe(10 + owned, 1)
    before = rig.status()
    rig.observe((110 if firmware else 10) + observed, 30)
    after = rig.status()
    assert after["device"]["activity_known"] and after["device"]["active"]
    assert after["test"]["state"] == "recovered_uncertain"
    assert len(after["history"]) == len(before["history"])
    assert after["test"]["capacity_mah"] == before["test"]["capacity_mah"]
    assert after["test"]["energy_wh"] == before["test"]["energy_wh"]
    if cycle:
        assert after["cycle"]["state"] == "interrupted"
    for capacity in range(31, 64 if sustained else 33):
        rig.observe(10 + observed, capacity)
        if sustained:
            time.sleep(1.9)
    # Even a return to the expected mode cannot reclaim ownership.
    rig.observe(10 + owned, 100)
    final = rig.status()
    assert final["test"]["state"] == "recovered_uncertain"
    assert final["history"] == before["history"] and rig.count(10) == 0
    assert final["test"]["capacity_mah"] == before["test"]["capacity_mah"]
    assert final["test"]["energy_wh"] == before["test"]["energy_wh"]
    rig.post("/test/stop")
    assert bytes(rig.wire[-10:]) == STOP
    rig.observe(owned)
    assert rig.status()["test"]["state"] == "stopped"
    if cycle:
        assert rig.status()["cycle"]["state"] == "interrupted"


def receive_batch(rig):
    rig.observe()
    step = dict(type="device", config=CONFIGS[0], completion="hardware")
    rig.post("/cycle/start", dict(recipe=dict(steps=[step, step], repeat_count=1)))
    rig.observe(10, 1)
    # The finite pre-next-step prefix must be reconciled before progression.
    # Active contradicts Settling even though zero-current Idle preceded it.
    os.write(rig.master, frame(20, 1) + frame(0, 1) + frame(10, 30))
    rig.wait(lambda: rig.status()["cycle"]["state"] == "interrupted", "batch contradiction interrupts Settling")
    status = rig.status()
    assert rig.count(1) == 1 and status["cycle"]["state"] == "interrupted"
    assert status["test"]["state"] == "recovered_uncertain" and status["device"]["activity_known"]
    rig.observe(0, 31)
    assert rig.count(1) == 1 and rig.status()["cycle"]["state"] == "interrupted"


def fragmented(rig, boundary):
    rig.observe()
    if boundary == "stop":
        rig.post("/test/start", dict(config=CONFIGS[0]))
        rig.observe(10, 1)
    partial = frame(10, 50)
    os.write(rig.master, partial[:18])
    time.sleep(.3)  # prefix crosses the actual serial receive service
    if boundary == "start":
        rig.post("/test/start", dict(config=CONFIGS[0]))
    elif boundary == "stop":
        rig.post("/test/stop")
    elif boundary == "expiry":
        time.sleep(10.1)
    elif boundary == "replacement":
        rig.post("/disconnect")
        rig.post("/connect")
    os.write(rig.master, partial[18:])
    time.sleep(.3)
    status = rig.status()
    assert status["test"]["state"] != "running", (boundary, status)
    if boundary in ("start", "expiry", "replacement"):
        assert not status["device"]["activity_known"], (boundary, status)
    assert len(status["history"]) == (1 if boundary == "stop" else 0)
    # Recovery parses a valid complete report without inheriting old provenance.
    rig.observe(0, 51)


def acquisition_deadline(rig, cycle):
    rig.observe()
    if cycle:
        step = dict(type="device", config=CONFIGS[0], completion="hardware")
        rig.post("/cycle/start", dict(recipe=dict(steps=[step], repeat_count=1)))
    else:
        rig.post("/test/start", dict(config=CONFIGS[0]))
    for capacity in range(1, 7):
        time.sleep(1.8)
        rig.observe(0, capacity)
    rig.observe(10, 50)
    status = rig.status()
    assert status["test"]["state"] == "recovered_uncertain" and status["history"] == []
    assert status["test"]["capacity_mah"] is None and rig.count(10) == 0
    if cycle:
        assert status["cycle"]["state"] == "interrupted"
    rig.post("/test/stop")


def firmware_closing(rig, cycle):
    rig.observe()
    if cycle:
        step = dict(type="device", config=CONFIGS[0], completion="hardware")
        rig.post("/cycle/start", dict(recipe=dict(steps=[step], repeat_count=1)))
    else:
        rig.post("/test/start", dict(config=CONFIGS[0]))
    rig.observe(10, 10)
    rig.observe(100, 11)
    before = rig.status()
    assert not before["capabilities"]["adjust"] and not before["capabilities"]["start"]
    rig.observe(10, 50)
    after = rig.status()
    assert after["test"]["state"] == "recovered_uncertain"
    assert after["history"] == before["history"] and after["test"]["capacity_mah"] == 11
    if cycle:
        assert after["cycle"]["state"] == "interrupted"
    rig.post("/test/stop")


def persistence_disconnect(rig):
    rig.observe()
    rig.post("/test/start", dict(config=CONFIGS[0]))
    # The actor has prepared metadata, but no sample writer is open yet. This
    # deterministic Linux sink fails the real buffered telemetry flush.
    samples = rig.directory / "data" / "samples.csv"
    samples.unlink()
    samples.symlink_to("/dev/full")
    rig.observe(10, 1)
    code, result = rig.request("/disconnect", post=True)
    assert code >= 400, (code, result)  # persistence error remains visible
    rig.wait(lambda: rig.count(6) == 1, "Disconnect despite telemetry flush failure")
    assert bytes(rig.wire[-20:]) == STOP + DISCONNECT
    status = rig.status()
    assert status["connection"] == "disconnected" and not status["device"]["activity_known"]


def reserved_controls(rig, phase):
    rig.observe()
    step = dict(type="device", config=CONFIGS[0], completion="hardware")
    recipe = dict(steps=[dict(type="rest", duration_seconds=30), step] if phase == "rest" else [step, step], repeat_count=1)
    code, saved = rig.request("/recipes", dict(name="software fixture", recipe=recipe), True)
    assert code == 200, (code, saved)
    rig.post("/cycle/start", dict(recipe=recipe))
    if phase == "settling":
        rig.observe(10, 1)
        rig.observe(20, 1)
        rig.wait(lambda: rig.status()["cycle"]["state"] == "settling", "Settling")
    before = rig.status()
    starts = rig.count(1)
    commands = [("/test/start", dict(config=CONFIGS[0])), ("/test/adjust", CONFIGS[0]),
                ("/test/resume", None), ("/cycle/start", dict(recipe=recipe)),
                (f"/recipes/{saved['id']}/start", {})]
    commands += [("/calibration", dict(operation=operation, **({} if value is None else dict(value=value))))
                 for operation, value in (("voltage_low", 1000), ("voltage_high", 4000),
                                          ("current_low", 100), ("current_high", 1000), ("confirm", None))]
    for path, body in commands:
        code, value = rig.request(path, body, True)
        assert code >= 400, (phase, path, code, value)
    after = rig.status()
    assert rig.count(1) == starts and rig.count(7) == rig.count(8) == rig.count(4) == 0
    assert after["cycle"]["state"] == ("resting" if phase == "rest" else "settling")
    assert after["current_run"] == before["current_run"] and after["history"] == before["history"]


def rest_contradiction(rig, state):
    rig.observe()
    step = dict(type="device", config=CONFIGS[0], completion="hardware")
    rig.post("/cycle/start", dict(recipe=dict(steps=[dict(type="rest", duration_seconds=1), step], repeat_count=1)))
    os.write(rig.master, frame(state, 50, 10))
    rig.wait(lambda: rig.status()["cycle"]["state"] == "interrupted", "Rest contradiction")
    rig.observe()
    time.sleep(1.2)
    assert rig.status()["cycle"]["state"] == "interrupted" and rig.count(1) == 0


def terminal_contradiction(rig, owned, observed, state, cycle):
    rig.observe(owned)
    if cycle:
        step = dict(type="device", config=CONFIGS[owned], completion="hardware")
        rig.post("/cycle/start", dict(recipe=dict(steps=[step, step], repeat_count=1)))
    else:
        rig.post("/test/start", dict(config=CONFIGS[owned]))
    rig.observe(10 + owned, 10)
    before = rig.status()
    rig.observe(state + observed, 50)
    after = rig.status()
    assert after["device"]["activity_known"] and not after["device"]["active"]
    assert after["test"]["state"] != "completed" and after["history"] == before["history"]
    assert after["test"]["capacity_mah"] == before["test"]["capacity_mah"]
    assert after["test"]["energy_wh"] == before["test"]["energy_wh"]
    if cycle:
        assert after["cycle"]["state"] == "interrupted"
    rig.observe(10 + owned, 51)
    assert rig.status()["test"]["state"] == "recovered_uncertain" and rig.count(10) == 0
    rig.post("/test/stop")


def continue_segments(rig):
    rig.observe()
    rig.post("/test/start", dict(config=CONFIGS[0]))
    rig.observe(10, 10)
    before = rig.status()["test"]
    time.sleep(10.2)
    rig.observe(0, 50)
    rig.post("/test/resume")
    rig.wait(lambda: rig.count(8) == 1, "actual Continue frame")
    rig.observe(10, 51)
    after = rig.status()
    assert after["device"]["capacity_mah"] == 51 and after["test"]["capacity_mah"] == 11
    assert after["test"]["energy_wh"] == before["energy_wh"]
    rig.post("/disconnect")
    with (rig.directory / "data" / "samples.csv").open() as samples:
        rows = list(csv.DictReader(samples))
    assert [int(row["capacity_mah"]) for row in rows] == [10, 11], rows


def closing_classification(rig):
    rig.observe()
    step = dict(type="device", config=CONFIGS[0], completion="hardware")
    rig.post("/cycle/start", dict(recipe=dict(steps=[step, step], repeat_count=1)))
    rig.observe(10, 10)
    rig.observe(100, 11)
    rig.observe(20, 12)
    assert rig.status()["cycle"]["state"] == "settling" and rig.count(1) == 1
    assert rig.status()["test"]["capacity_mah"] == 11
    os.write(rig.master, frame(100, 12))
    rig.wait(lambda: rig.count(1) == 2, "later firmware settles before next Start")
    assert rig.status()["test"]["state"] == "starting"


def archived_capacity(rig, run_id, capacity, sample_count, state):
    code, runs = rig.request("/runs")
    assert code == 200, (code, runs)
    summary = next(run for run in runs if run["id"] == run_id)
    assert summary["capacity_mah"] == capacity, summary
    assert summary["sample_count"] == sample_count and summary["state"] == state, summary
    code, detail = rig.request("/runs/" + run_id)
    assert code == 200 and detail["summary"] == summary, (code, detail)
    assert len(detail["samples"]) == sample_count, detail
    disk = json.loads((rig.directory / "data/runs" / f"{run_id}.json").read_text())
    assert disk == summary, disk
    return summary


def unowned_archive(rig, cycle):
    rig.observe(0, 40)
    if cycle:
        step = dict(type="device", config=CONFIGS[0], completion="hardware")
        rig.post("/cycle/start", dict(recipe=dict(steps=[step], repeat_count=1)))
    else:
        rig.post("/test/start", dict(config=CONFIGS[0]))
    run_id = rig.status()["current_run"]["id"]
    rig.observe(11, 50)  # First Active contradicts CC: never owned.
    active = rig.status()
    assert active["test"]["state"] == "recovered_uncertain"
    assert active["test"]["capacity_mah"] is None and active["history"] == []
    rig.observe(1, 51)
    live = rig.status()
    assert live["test"]["state"] == "stopped"
    assert live["test"]["result"] == "physical device mode contradicts the owned test"
    assert live["test"]["capacity_mah"] is None and live["history"] == []
    assert live["device"]["capacity_mah"] == 51 and rig.count(10) == 0
    if cycle:
        assert live["cycle"]["state"] == "interrupted"
        execution_id = live["cycle"]["execution_id"]
        assert rig.request("/runs")[1] == []  # Actual interrupted-child boundary.
        rig.post("/test/start", dict(config=CONFIGS[0]))
    summary = archived_capacity(rig, run_id, None, 0, "stopped")
    assert summary["config"] == CONFIGS[0]
    if cycle:
        assert summary["name"] is None
        assert summary["cycle"] == dict(execution_id=execution_id, repeat_index=0, step_index=0)
        code, parent = rig.request("/cycles/" + execution_id)
        assert code == 200 and parent["child_runs"] == [summary], parent
        assert parent["summary"]["child_run_count"] == 1
        assert all(row["test_capacity_mah"] is None for row in parent["samples"])
        # New manual work is independent of the interrupted parent and child.
        later_id = rig.status()["current_run"]["id"]
        assert later_id != run_id
        rig.observe(10, 7)
        rig.observe(20, 8)
        later = archived_capacity(rig, later_id, 8, 1, "completed")
        assert later["cycle"] is None
    else:
        assert summary["cycle"] is None
    rig.restart()
    assert archived_capacity(rig, run_id, None, 0, "stopped") == summary
    if cycle:
        assert rig.request("/cycles/" + execution_id)[1]["child_runs"] == [summary]
        assert archived_capacity(rig, later_id, 8, 1, "completed") == later
    else:
        assert rig.status()["test"]["capacity_mah"] is None


def firmware_only_archive(rig, cycle, capacity):
    rig.observe(0, 40)
    if cycle:
        step = dict(type="device", config=CONFIGS[0], completion="hardware")
        rig.post("/cycle/start", dict(recipe=dict(steps=[step], repeat_count=1)))
    else:
        rig.post("/test/start", dict(config=CONFIGS[0]))
    run_id = rig.status()["current_run"]["id"]
    rig.observe(110, capacity)
    active = rig.status()
    assert active["test"]["state"] == "running" and active["history"] == []
    assert active["test"]["capacity_mah"] == capacity
    closing = capacity + (1 if capacity else 0)
    rig.observe(100, closing)  # Legitimately attributed closing input, still no row.
    assert rig.status()["test"]["capacity_mah"] == closing
    rig.observe(20, 50)  # Resolve reason, not an additional attributable counter.
    assert rig.status()["test"]["state"] == "completed"
    assert rig.status()["test"]["capacity_mah"] == closing and rig.status()["history"] == []
    rig.observe(0, 51)
    summary = archived_capacity(rig, run_id, closing, 0, "completed")
    if cycle:
        assert rig.status()["cycle"]["state"] == "completed"
        assert rig.request("/cycles/" + summary["cycle"]["execution_id"])[1]["child_runs"] == [summary]
    rig.restart()
    assert archived_capacity(rig, run_id, closing, 0, "completed") == summary
    assert rig.status()["test"]["capacity_mah"] == closing


def terminal_capacity_archive(rig, cycle):
    rig.observe()
    if cycle:
        step = dict(type="device", config=CONFIGS[0], completion="hardware")
        rig.post("/cycle/start", dict(recipe=dict(steps=[step], repeat_count=1)))
    else:
        rig.post("/test/start", dict(config=CONFIGS[0]))
    run_id = rig.status()["current_run"]["id"]
    rig.observe(10, 10)
    for state, capacity, current in [(20, 12, 10), (0, 13, 10), (0, 14, 0)]:
        os.write(rig.master, frame(state, capacity, current))
        rig.wait(lambda: rig.status()["device"]["capacity_mah"] == capacity, "terminal counter")
        status = rig.status()
        assert status["test"]["state"] == "completed" and status["test"]["capacity_mah"] == 12
        assert len(status["history"]) == 1
        if cycle:
            assert status["cycle"]["state"] == ("completed" if current == 0 else "settling")
    summary = archived_capacity(rig, run_id, 12, 1, "completed")
    rig.restart()
    assert archived_capacity(rig, run_id, 12, 1, "completed") == summary
    assert rig.status()["test"]["capacity_mah"] == 12


def continue_capacity_archive(rig):
    rig.observe()
    rig.post("/test/start", dict(config=CONFIGS[0]))
    run_id = rig.status()["current_run"]["id"]
    rig.observe(10, 10)
    rig.post("/test/stop")
    rig.observe(0, 10)
    archived_capacity(rig, run_id, 10, 1, "stopped")
    rig.observe(0, 50)  # Unowned growth must not be imported by explicit Continue.
    assert rig.status()["test"]["capacity_mah"] == 10
    rig.post("/test/resume")
    rig.wait(lambda: rig.count(8) == 1, "actual Continue")
    rig.observe(10, 51)
    status = rig.status()
    assert status["current_run"]["id"] == run_id and status["test"]["capacity_mah"] == 11
    rig.post("/test/stop")
    rig.observe(0, 51)
    summary = archived_capacity(rig, run_id, 11, 2, "stopped")
    assert [row["capacity_mah"] for row in rig.request("/runs/" + run_id)[1]["samples"]] == [10, 11]
    rig.restart()
    assert archived_capacity(rig, run_id, 11, 2, "stopped") == summary
    assert rig.status()["test"]["capacity_mah"] == 11


def main():
    binary = str(pathlib.Path(sys.argv[1]).resolve())
    scratch = pathlib.Path(sys.argv[2]).resolve()
    scratch.mkdir(parents=True, exist_ok=False)
    cases = [(f"stop-{state}", safety_stop, (state,)) for state in ("unknown", "idle", "stopped", "completed")]
    cases += [(f"mode-{a}-{b}-{fw}-{cycle}-{first}", contradiction, (a, b, fw, cycle, first))
              for a in range(3) for b in range(3) if a != b for fw in (False, True)
              for cycle in (False, True) for first in (False, True)]
    cases.append(("sustained-cc-cp", contradiction, (0, 1, False, True, False, True)))
    cases.append(("pre-command-serial-batch", receive_batch, ()))
    cases += [(f"fragment-{boundary}", fragmented, (boundary,)) for boundary in ("start", "stop", "expiry", "replacement")]
    cases += [(f"acquisition-{cycle}", acquisition_deadline, (cycle,)) for cycle in (False, True)]
    cases += [(f"firmware-closing-{cycle}", firmware_closing, (cycle,)) for cycle in (False, True)]
    cases.append(("persistence-disconnect", persistence_disconnect, ()))
    cases += [(f"reserved-{phase}", reserved_controls, (phase,)) for phase in ("rest", "settling")]
    cases += [(f"rest-contradiction-{state}", rest_contradiction, (state,)) for state in (0, 10, 100, 110)]
    cases += [(f"terminal-{a}-{b}-{state}-{cycle}", terminal_contradiction, (a, b, state, cycle))
              for a in range(3) for b in range(3) if a != b for state in (0, 20, 100) for cycle in (False, True)]
    cases.append(("continue-segments", continue_segments, ()))
    cases.append(("closing-classification", closing_classification, ()))
    cases += [(f"unowned-archive-{cycle}", unowned_archive, (cycle,)) for cycle in (False, True)]
    cases += [(f"firmware-only-archive-{cycle}-{capacity}", firmware_only_archive, (cycle, capacity))
              for cycle in (False, True) for capacity in (0, 10)]
    cases += [(f"terminal-capacity-archive-{cycle}", terminal_capacity_archive, (cycle,)) for cycle in (False, True)]
    cases.append(("continue-capacity-archive", continue_capacity_archive, ()))

    def run(case):
        name, test, args = case
        rig = Rig(binary, scratch / name)
        try:
            test(rig, *args)
            print("PASS", name, flush=True)
        finally:
            rig.close()

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(run, cases))
    print(f"{len(cases)} production HTTP/PTY regressions passed")


if __name__ == "__main__":
    main()
