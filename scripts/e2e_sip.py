#!/usr/bin/env python3
"""
End-to-end SIP tests for active-call, driven by `sipbot`.

Starts a real active-call server (webhook invite handler) plus a local
webhook receiver, then exercises complete call flows through real
SIP/RTP signalling:

  1. options     - SIP OPTIONS ping
  2. basic_call  - INVITE -> accept over WS -> DTMF -> hold/resume -> BYE
  3. cancel      - CANCEL before answer, call torn down cleanly
  4. reject      - python rejects with 486, sipbot sees failure
  5. soak        - N concurrent calls (accept + hangup), success-rate and
                   server-RSS growth assertions (memory leak check)

Usage:
    python3 scripts/e2e_sip.py [--release] [--sipbot PATH] [--binary PATH]
                               [--skip-soak] [-v]

Requires: sipbot (cargo install sipbot), python3 + websockets.
"""

import argparse
import asyncio
import json
import os
import queue
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #


def free_port() -> int:
    import socket

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def free_udp_port() -> int:
    """A UDP port that is bindable right now."""
    import socket

    for _ in range(20):
        port = random.randint(20000, 60000)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            try:
                s.bind(("0.0.0.0", port))
                return port
            except OSError:
                continue
    raise RuntimeError("no free UDP port found")


def rss_kb(pid: int) -> int:
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
    )
    try:
        return int(out.stdout.strip())
    except ValueError:
        return 0


class WebhookServer:
    """Receives the server's invite notifications and queues them."""

    def __init__(self, port: int):
        self.port = port
        self.invites: "queue.Queue[dict]" = queue.Queue()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length", 0))
                payload = json.loads(self.rfile.read(length) or b"{}")
                if payload.get("event") == "invite":
                    outer.invites.put(payload)
                body = json.dumps({"status": "ok"}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):  # silence
                pass

        self.httpd = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def start(self):
        self.thread.start()

    def stop(self):
        self.httpd.shutdown()


async def wait_for_http(url: str, timeout: float = 15.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as resp:
                if resp.status == 200:
                    return
        except (urllib.error.URLError, ConnectionError, OSError):
            pass
        await asyncio.sleep(0.3)
    raise TimeoutError(f"server did not become ready at {url}")


async def list_calls(http_port: int) -> list:
    with urllib.request.urlopen(f"http://127.0.0.1:{http_port}/list", timeout=5) as r:
        return json.loads(r.read()).get("active_calls", [])


async def wait_list_empty(http_port: int, timeout: float = 15.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if not await list_calls(http_port):
                return
        except Exception:
            pass
        await asyncio.sleep(0.3)
    raise TimeoutError("active_calls registry did not drain to empty")


async def next_invite(webhook: WebhookServer, timeout: float = 15.0) -> dict:
    """Wait for the next invite notification (retries around the blocking get)."""
    loop = asyncio.get_running_loop()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            return await loop.run_in_executor(None, webhook.invites.get, True, 0.5)
        except queue.Empty:
            continue
    raise TimeoutError("no invite webhook received")


def run_sipbot(args: list, timeout: float = 60.0) -> subprocess.Popen:
    cmd = [SIPBOT, *args]
    if VERBOSE:
        print(f"    $ {' '.join(cmd)}")
    return subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )


def collect(proc: subprocess.Popen, timeout: float = 60.0) -> tuple[int, str]:
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        out, err = proc.communicate()
        return -1, out + err
    return proc.returncode, out + err


class WsCall:
    """One accepted call: WebSocket control channel + event collection."""

    def __init__(self, http_port: int, dialog_id: str):
        self.url = f"ws://127.0.0.1:{http_port}/call/sip?id={dialog_id}"
        self.ws = None
        self.events: list[dict] = []

    async def connect(self):
        import websockets

        self.ws = await websockets.connect(self.url, open_timeout=10)
        return self

    async def send(self, command: dict):
        await self.ws.send(json.dumps(command))

    async def pump(self, duration: float):
        """Collect events for `duration` seconds (tolerates socket close)."""
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            try:
                timeout = max(0.05, deadline - time.monotonic())
                msg = await asyncio.wait_for(self.ws.recv(), timeout=timeout)
            except (asyncio.TimeoutError, TimeoutError):
                break
            except Exception:
                break  # socket closed
            if isinstance(msg, bytes):
                continue
            self.events.append(json.loads(msg))

    async def wait_event(self, name: str, timeout: float = 15.0) -> dict | None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                msg = await asyncio.wait_for(
                    self.ws.recv(), timeout=max(0.05, deadline - time.monotonic())
                )
            except (asyncio.TimeoutError, TimeoutError):
                break
            except Exception:
                break  # socket closed
            if isinstance(msg, bytes):
                continue
            event = json.loads(msg)
            self.events.append(event)
            if event.get("event") == name:
                return event
        return None

    def has_event(self, name: str, pred=None) -> dict | None:
        for e in self.events:
            if e.get("event") == name and (pred is None or pred(e)):
                return e
        return None

    async def close(self):
        if self.ws is not None:
            await self.ws.close()


# --------------------------------------------------------------------------- #
# scenarios
# --------------------------------------------------------------------------- #


async def scenario_options(ctx) -> bool:
    print("[scenario] OPTIONS ping")
    sip_port = ctx["sip_port"]
    proc = run_sipbot(["options", f"sip:100@127.0.0.1:{sip_port}", "-v"])
    code, output = collect(proc, timeout=20)
    ok = code == 0 and "200" in output
    if not ok:
        print(f"    FAIL rc={code}\n{output[-2000:]}")
    return ok


async def scenario_basic_call(ctx) -> bool:
    print("[scenario] basic call: accept + DTMF + hold/resume + BYE")
    sip_port, http_port = ctx["sip_port"], ctx["http_port"]
    proc = run_sipbot(
        [
            "call",
            "--target",
            f"sip:e2e@127.0.0.1:{sip_port}",
            "--external",
            "127.0.0.1",
            "--hangup",
            "10",
            "--dtmf-flows",
            "2s:5",
            # sipbot flow delays are relative to the previous entry:
            # hold at +3s, resume at +6s, BYE at +10s.
            "--reinvite-flows",
            "3s:hold,3s:resume",
            "-v",
        ]
    )

    invite = await next_invite(ctx["webhook"], timeout=15)
    dialog_id = invite["dialogId"]
    print(f"    invite received: {dialog_id}")

    call = WsCall(http_port, dialog_id)
    await call.connect()
    await call.send({"command": "accept", "option": {}})

    answer = await call.wait_event("answer", timeout=10)
    ok = answer is not None and bool(answer.get("sdp"))
    print(f"    answer sdp: {len(answer.get('sdp', '')) if answer else 0} bytes")

    # DTMF arrives ~2s, hold ~4s, resume ~6s, sipbot BYE at 10s.
    await call.pump(12.5)

    dtmf = call.has_event("dtmf", lambda e: e.get("digit") == "5")
    hold_on = call.has_event("hold", lambda e: e.get("onHold") is True)
    hold_off = call.has_event("hold", lambda e: e.get("onHold") is False)
    hangup = call.has_event("hangup")
    print(
        f"    dtmf5={bool(dtmf)} holdOn={bool(hold_on)} "
        f"holdOff={bool(hold_off)} hangup={bool(hangup)}"
    )

    code, output = collect(proc, timeout=20)
    established = "Call established" in output or "200 OK" in output
    print(f"    sipbot rc={code} established={established}")

    await call.close()
    drained = True
    try:
        await wait_list_empty(http_port, timeout=10)
    except TimeoutError:
        drained = False

    ok = ok and dtmf and hold_on and hold_off and hangup and code == 0 and established and drained
    if not ok:
        print(f"    FAIL events={[e.get('event') for e in call.events]}")
    return ok


async def scenario_cancel(ctx) -> bool:
    print("[scenario] CANCEL before answer")
    sip_port, http_port = ctx["sip_port"], ctx["http_port"]
    proc = run_sipbot(
        [
            "call",
            "--target",
            f"sip:e2e@127.0.0.1:{sip_port}",
            "--external",
            "127.0.0.1",
            "--cancel-prob",
            "100",
            "--hangup",
            "8",
            "-v",
        ]
    )

    invite = await next_invite(ctx["webhook"], timeout=15)
    dialog_id = invite["dialogId"]
    print(f"    invite received: {dialog_id} (ringing, not answering)")
    # sipbot only CANCELs from Trying/Early states; send 180 Ringing so its
    # cancel path triggers, then do not answer.
    call = WsCall(http_port, dialog_id)
    await call.connect()
    await call.send({"command": "ringing", "recorder": None, "early_media": False, "ringtone": None})
    # sipbot cancels on the 180 and exits on its own.
    code, output = collect(proc, timeout=30)
    print(f"    sipbot rc={code}")
    cancelled = "cancel" in output.lower()
    await call.close()

    drained = True
    try:
        await wait_list_empty(http_port, timeout=10)
    except TimeoutError:
        drained = False

    ok = code == 0 and cancelled and drained
    if not ok:
        print(f"    FAIL cancelled={cancelled} drained={drained}\n{output[-1500:]}")
    return ok


async def scenario_reject(ctx) -> bool:
    print("[scenario] reject with 486")
    sip_port, http_port = ctx["sip_port"], ctx["http_port"]
    proc = run_sipbot(
        [
            "call",
            "--target",
            f"sip:e2e@127.0.0.1:{sip_port}",
            "--external",
            "127.0.0.1",
            "--hangup",
            "10",
            "-v",
        ]
    )

    invite = await next_invite(ctx["webhook"], timeout=15)
    dialog_id = invite["dialogId"]
    call = WsCall(http_port, dialog_id)
    await call.connect()
    await call.send({"command": "reject", "reason": "busy", "code": 486})

    code, output = collect(proc, timeout=20)
    rejected = "486" in output or "Busy" in output
    print(f"    sipbot rc={code} rejected={rejected}")

    await call.close()
    drained = True
    try:
        await wait_list_empty(http_port, timeout=10)
    except TimeoutError:
        drained = False

    ok = rejected and drained
    if not ok:
        print(f"    FAIL rejected={rejected} drained={drained}\n{output[-1500:]}")
    return ok


async def scenario_soak(ctx, total: int = 10, cps: float = 2.0) -> bool:
    print(f"[scenario] soak: {total} calls @ {cps} cps (accept+hangup, RSS check)")

    async def one_round(label: str) -> bool:
        sip_port, http_port = ctx["sip_port"], ctx["http_port"]
        rss_before = rss_kb(ctx["server"].pid)
        hangup_count = 0

        async def _one_call(port, dialog_id):
            call = WsCall(port, dialog_id)
            await call.connect()
            await call.send({"command": "accept", "option": {}})
            await call.wait_event("hangup", timeout=30)
            await call.close()
            return 1 if call.has_event("hangup") else 0

        async def handle_dialogs():
            nonlocal hangup_count
            tasks = []
            deadline = time.monotonic() + total / cps + 15
            accepted = 0
            while accepted < total and time.monotonic() < deadline:
                try:
                    invite = await next_invite(ctx["webhook"], timeout=5)
                except (asyncio.TimeoutError, TimeoutError):
                    continue
                accepted += 1
                tasks.append(
                    asyncio.create_task(_one_call(http_port, invite["dialogId"]))
                )
            results = await asyncio.gather(*tasks, return_exceptions=True)
            hangup_count = sum(1 for r in results if isinstance(r, int))

        proc = run_sipbot(
            [
                "call",
                "--target",
                f"sip:e2e@127.0.0.1:{sip_port}",
                "--external",
                "127.0.0.1",
                "--hangup",
                "3",
                "--total",
                str(total),
                "--cps",
                str(int(cps)),
                "-v",
            ]
        )

        await handle_dialogs()
        code, output = collect(proc, timeout=total / cps + 30)
        print(f"    [{label}] sipbot rc={code}, hangup events: {hangup_count}/{total}")
        if code != 0:
            print(f"    [{label}] sipbot output tail:\n{output[-1500:]}")

        try:
            await wait_list_empty(http_port, timeout=15)
            drained = True
        except TimeoutError:
            drained = False

        # Let the allocator settle, then measure.
        await asyncio.sleep(3)
        rss_after = rss_kb(ctx["server"].pid)
        growth = rss_after - rss_before
        per_call = growth / max(total, 1)
        print(
            f"    [{label}] server rss: {rss_before}KB -> {rss_after}KB "
            f"(+{growth}KB, {per_call:.0f}KB/call)"
        )
        return code == 0 and hangup_count == total and drained, per_call

    # Round 1 warms everything up (first-touch page faults, allocator arenas);
    # round 2 measures steady-state growth. A real per-call leak shows up in
    # round 2; one-off retention does not.
    ok1, per_call1 = await one_round("warmup")
    ok2, per_call2 = await one_round("measure")
    leak_ok = per_call2 < 768
    print(
        f"    growth: warmup {per_call1:.0f}KB/call, measure {per_call2:.0f}KB/call "
        f"(leak threshold 768KB/call)"
    )
    ok = ok1 and ok2 and leak_ok
    if not ok:
        print(f"    FAIL warmup_ok={ok1} measure_ok={ok2} leak_ok={leak_ok}")
    return ok


# --------------------------------------------------------------------------- #
# server management
# --------------------------------------------------------------------------- #


def build_server(release: bool) -> Path:
    binary = REPO / ("target/release/active-call" if release else "target/debug/active-call")
    if not binary.exists():
        profile = "--release" if release else ""
        print(f"building server ({'release' if release else 'debug'})...")
        subprocess.run(
            f"cargo build {profile}".split(), cwd=REPO, check=True
        )
    return binary


def start_server(binary: Path, workdir: Path, ports: dict) -> subprocess.Popen:
    config = f"""
addr = "127.0.0.1"
udp_port = {ports['sip']}
http_addr = "127.0.0.1:{ports['http']}"
accept_timeout = "120s"
media_cache_path = "{workdir}/media_cache"
rtp_start_port = {ports['rtp0']}
rtp_end_port = {ports['rtp1']}
graceful_shutdown = true
log_level = "%s"

[handler]
type = "webhook"
url = "http://127.0.0.1:%d/invite"
method = "POST"
""" % (
        os.environ.get("E2E_LOG_LEVEL", "info"),
        ports["webhook"],
    )
    conf_path = workdir / "e2e.toml"
    conf_path.write_text(config)

    log = open(workdir / "server.log", "w")
    proc = subprocess.Popen(
        [
            str(binary),
            "--conf",
            str(conf_path),
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
        cwd=workdir,
    )
    return proc


# --------------------------------------------------------------------------- #
# main
# --------------------------------------------------------------------------- #

SIPBOT = shutil.which("sipbot") or shutil.which("sipbot.exe") or "sipbot"
VERBOSE = False


async def main() -> int:
    global VERBOSE
    parser = argparse.ArgumentParser()
    parser.add_argument("--release", action="store_true", help="use release build")
    parser.add_argument("--sipbot", help="path to sipbot binary")
    parser.add_argument("--binary", help="path to active-call binary")
    parser.add_argument("--skip-soak", action="store_true")
    parser.add_argument(
        "--only", help="comma-separated scenario names to run (options,basic_call,cancel,reject,soak)"
    )
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    VERBOSE = args.verbose
    if args.sipbot:
        global SIPBOT
        SIPBOT = args.sipbot

    if shutil.which(SIPBOT) is None and not Path(SIPBOT).exists():
        print(f"sipbot not found: {SIPBOT} (cargo install sipbot)")
        return 2
    try:
        import websockets  # noqa: F401
    except ImportError:
        print("python 'websockets' package required: pip install websockets")
        return 2

    binary = Path(args.binary) if args.binary else build_server(args.release)

    workdir = Path(tempfile.mkdtemp(prefix="active-call-e2e-"))
    ports = {
        "sip": free_udp_port(),
        "http": free_port(),
        "webhook": free_port(),
    }
    ports["rtp0"] = 30000 + random.randint(0, 200) * 50
    ports["rtp1"] = ports["rtp0"] + 2000

    webhook = WebhookServer(ports["webhook"])
    webhook.start()

    server = start_server(binary, workdir, ports)
    ctx = {"sip_port": ports["sip"], "http_port": ports["http"], "webhook": webhook, "server": server}

    results: list[tuple[str, bool, str]] = []
    try:
        await wait_for_http(f"http://127.0.0.1:{ports['http']}/list")
        print(
            f"server ready: sip={ports['sip']} http={ports['http']} "
            f"webhook={ports['webhook']} log={workdir}/server.log"
        )

        selected = set(args.only.split(",")) if args.only else None

        for name, coro in [
            ("options", scenario_options(ctx)),
            ("basic_call", scenario_basic_call(ctx)),
            ("cancel", scenario_cancel(ctx)),
            ("reject", scenario_reject(ctx)),
        ]:
            if selected is not None and name not in selected:
                continue
            t0 = time.monotonic()
            try:
                ok = await coro
                err = ""
            except Exception as e:  # noqa: BLE001
                ok, err = False, f"{type(e).__name__}: {e}"
            results.append((name, ok, err))
            print(f"    => {'PASS' if ok else 'FAIL'} in {time.monotonic()-t0:.1f}s")

        if not args.skip_soak and (selected is None or "soak" in selected):
            t0 = time.monotonic()
            try:
                ok = await scenario_soak(ctx)
                err = ""
            except Exception as e:  # noqa: BLE001
                ok, err = False, f"{type(e).__name__}: {e}"
            results.append(("soak", ok, err))
            print(f"    => {'PASS' if ok else 'FAIL'} in {time.monotonic()-t0:.1f}s")

    finally:
        server.send_signal(signal.SIGTERM)
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
        webhook.stop()

    print("\n===== E2E summary =====")
    width = max(len(n) for n, _, _ in results)
    failed = 0
    for name, ok, err in results:
        mark = "PASS" if ok else "FAIL"
        print(f"  {name:<{width}}  {mark}  {err}")
        failed += 0 if ok else 1
    print(f"  {'total':<{width}}  {len(results)-failed}/{len(results)} passed")
    print(f"  workdir: {workdir}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
