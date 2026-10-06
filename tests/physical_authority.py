"""Production server/HTTP/PTY regressions. Never opens a real serial device.

Usage: python tests/physical_authority.py <ebc-server> <new scratch directory>
"""
import concurrent.futures
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
STOP = bytes.fromhex("fa0200000000000002f8")
DISCONNECT = bytes.fromhex("fa0600000000000006f8")


def frame(state, capacity=0):
    active = state in (10, 11, 12, 110, 111, 112)
    payload = [state, 0, 10 if active else 0, *divmod(4000, 240), *divmod(capacity, 240), 0, 0, 0, 10, 1, 60, 0, 0, 9]
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
        code, value = self.request(path, body, True)
        assert code == 200, (path, code, value)
        time.sleep(.05)
        return value

    def status(self):
        return self.request("/status")[1]

    def observe(self, state=0, capacity=0):
        os.write(self.master, frame(state, capacity))
        time.sleep(.08)

    def count(self, command):
        return sum(self.wire[i + 1] == command for i in range(0, len(self.wire) - 9, 10))

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
            assert rig.status()["test"]["state"] == state
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
    # All three frames are received before the next Start is written. The final
    # Active must not acknowledge the next child merely because it is processed
    # after that write while iterating the already-parsed serial batch.
    os.write(rig.master, frame(20, 1) + frame(0, 1) + frame(10, 30))
    time.sleep(.2)
    status = rig.status()
    assert rig.count(1) == 2 and status["cycle"]["state"] == "starting_step"
    assert status["test"]["state"] == "starting" and not status["device"]["activity_known"]
    assert status["history"] == []
    rig.observe(10, 1)
    assert rig.status()["test"]["state"] == "running"


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
