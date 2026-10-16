#!/usr/bin/env python3
"""Compare JIT backends through the VM shell API (Python standard library only)."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def positive(value):
    n = int(value)
    if n <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return n


def arguments():
    root = Path(__file__).resolve().parents[1]
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Example (build release binaries with both backends first):
  python3 scripts/jit-bench.py \\
    --workdir /home/user/workspace/riscv/cmd/goemu \\
    --config /home/user/workspace/repo/configs/vmagent.goemu \\
    --repetitions 3 --output jit-bench.json

Each repetition launches each backend in alternating order, with empty,
isolated state/cache directories. Cold readiness includes Linux, agent,
optional warmup, base snapshot and cache saving; it is NOT disk-cache-cold
boot time. Workloads run first from the base, then repeatedly along their
own continuation chain (new Node process each time). -warm 0 always forces
VM restores. Timings use host monotonic wall time, not the guest clock.
Exec total includes restore, command, snapshot and complete NDJSON response;
exit_seconds ends at the exit event and excludes later snapshot completion.
integer_loop_hot is host time between streamed markers around the 300M-loop
body after a 1M-loop warmup, excluding Node startup and snapshot overhead.
Warmup is identical for both backends; use --warmup '' to disable it.
No benchmarks are run by --help. All temporary state/logs are removed.
""")
    p.add_argument("--server", type=Path, default=root / "target/release/rsemu-server")
    p.add_argument("--rsemu", type=Path, default=root / "target/release/rsemu")
    p.add_argument("--config", type=Path, required=True)
    p.add_argument("--workdir", type=Path, required=True)
    p.add_argument("--repetitions", type=positive, default=3)
    p.add_argument("--repeats", type=positive, default=2,
                   help="additional commands on each fresh command's continuation (default 2)")
    p.add_argument("--startup-timeout", type=positive, default=600)
    p.add_argument("--command-timeout", type=positive, default=120)
    p.add_argument("--warmup", default="node -e 'console.log(42)' ",
                   help="base VM warmup, included in readiness timing")
    p.add_argument("--output", type=Path, help="also write JSON report to this file")
    return p.parse_args()


def workloads():
    # Sum 0..N-1, reduced modulo 2^32: independent closed-form oracle.
    n = 300_000_000
    integer = (
        "function run(n){let s=0;for(let i=0;i<n;i++)s=(s+i)|0;return s>>>0;}"
        f"run(1000000);console.log('hot-start');console.log('hot-end:'+run({n}))"
    )
    # Exact compact JSON bytes are generated independently in Python.
    rows = [{"i": i, "v": (i * 17) % 1009} for i in range(4000)]
    raw = json.dumps(rows, separators=(",", ":")).encode()
    digest = hashlib.sha256(raw).hexdigest()
    crypto = (
        "const c=require('crypto');const a=Array.from({length:4000},"
        "(_,i)=>({i,v:(i*17)%1009}));let s=JSON.stringify(a);"
        "for(let i=0;i<16;i++){const b=JSON.parse(s);s=JSON.stringify(b);}"
        "let h;for(let i=0;i<32;i++)h=c.createHash('sha256').update(s).digest('hex');"
        "console.log(JSON.parse(s).length+':'+h)"
    )
    return [("node_startup", "console.log('node-ready:'+6*7)", "node-ready:42\n"),
            ("integer_loop", integer, f"hot-start\nhot-end:{(n * (n - 1) // 2) % (2**32)}\n"),
            ("crypto_json", crypto, f"4000:{digest}\n")]


# Do not let proxy environment variables redirect loopback benchmark traffic.
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def execute(url, command, continuation, timeout):
    body = {"command": command, "timeout": timeout}
    if continuation is not None:
        body["continuation"] = continuation
    request = urllib.request.Request(url + "/v1/exec", json.dumps(body).encode(),
                                     {"Content-Type": "application/json"})
    events, streams = [], {"stdout": bytearray(), "stderr": bytearray()}
    start = time.perf_counter()
    exit_seconds = None
    code = None
    next_id = None
    hot_start = None
    hot_end = None
    with HTTP.open(request, timeout=timeout + 150) as response:
        for line in response:
            if time.perf_counter() - start > timeout + 150:
                raise RuntimeError("exec response exceeded wall-clock deadline")
            event = json.loads(line)
            events.append(event)
            kind = event.get("type")
            if next_id is not None:
                raise RuntimeError("event after continuation")
            if kind in streams:
                data = (base64.b64decode(event["data_base64"], validate=True)
                        if "data_base64" in event else event["data"].encode())
                streams[kind].extend(data)
                if kind == "stdout":
                    if hot_start is None and b"hot-start\n" in streams[kind]:
                        hot_start = time.perf_counter() - start
                    if hot_end is None and b"hot-end:" in streams[kind]:
                        hot_end = time.perf_counter() - start
            elif kind == "exit" and exit_seconds is None:
                code = event["code"]
                exit_seconds = time.perf_counter() - start
            elif kind == "continuation" and exit_seconds is not None:
                next_id = event["id"]
                if not isinstance(next_id, str) or not next_id:
                    raise RuntimeError("invalid continuation id")
            else:
                raise RuntimeError(f"unexpected exec event: {event!r}")
    result = {"seconds": time.perf_counter() - start, "exit_seconds": exit_seconds,
              "hot_seconds": None if hot_end is None else hot_end - hot_start,
              "exit_code": code, "continuation": next_id, "events": events,
              **{k: bytes(v).decode("utf-8", errors="replace") for k, v in streams.items()}}
    if code != 0 or next_id is None:
        raise RuntimeError(f"unsuccessful/incomplete exec: {result!r}")
    return result


def stop_group(process):
    # A new session isolates this server AND its spawned VMs from the harness.
    # Kill the group even if its leader has already exited.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def trial(args, backend, repetition, directory):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    # There is a small bind race; readiness/process checks make collisions fail.
    url = f"http://127.0.0.1:{port}"
    command = [str(args.server), "-rsemu", str(args.rsemu), "-config", str(args.config),
               "-state", str(directory / "state"), "-listen", f"127.0.0.1:{port}",
               "-jit-backend", backend, "-warm", "0", "-timeout", str(args.command_timeout)]
    if args.warmup:
        command += ["-warmup", args.warmup]
    with (directory / "server.log").open("w+") as log:
        start = time.perf_counter()
        process = subprocess.Popen(command, cwd=args.workdir, stdout=log, stderr=log,
                                   stdin=subprocess.DEVNULL, start_new_session=True)
        try:
            while True:
                if process.poll() is not None:
                    raise RuntimeError(f"server exited with status {process.returncode}")
                if time.perf_counter() - start > args.startup_timeout:
                    raise RuntimeError("server readiness timeout")
                try:
                    with HTTP.open(url + "/v1/health", timeout=1) as response:
                        health = json.load(response)
                    if health.get("status") == "ok" and health.get("warm") == 0:
                        break
                except (OSError, ValueError, urllib.error.URLError):
                    pass
                time.sleep(0.05)
            result = {"backend": backend, "repetition": repetition,
                      "launch_command": command, "startup_seconds": time.perf_counter() - start,
                      "health": health, "measurements": []}
            for name, js, expected in workloads():
                continuation = None
                for index in range(args.repeats + 1):
                    shell = "node -e " + shlex.quote(js)
                    sample = execute(url, shell, continuation, args.command_timeout)
                    if sample["stdout"] != expected:
                        raise RuntimeError(f"{name}: expected {expected!r}, got {sample!r}")
                    if name == "integer_loop" and not sample["hot_seconds"]:
                        raise RuntimeError("loop markers were buffered together; hot time is unavailable")
                    sample.update(workload=name, mode="fresh" if index == 0 else "continued",
                                  repeat_index=index, command=shell, expected_stdout=expected,
                                  input_continuation=continuation)
                    result["measurements"].append(sample)
                    continuation = sample["continuation"]
            return result
        except BaseException:
            log.flush()
            log.seek(0)
            print(log.read()[-16000:], file=sys.stderr)
            raise
        finally:
            stop_group(process)


def main():
    args = arguments()
    if os.name != "posix":
        raise RuntimeError("POSIX process groups are required")
    for name in ("server", "rsemu", "config", "workdir"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    report = {"schema_version": 1, "host": platform.platform(),
              "python": platform.python_version(), "settings": vars(args).copy(),
              "notes": "Empty per-trial backend caches; host OS caches not flushed. "
                       "Startup includes warmup/snapshot. Exec includes restore/snapshot. "
                       "Continued commands start new Node processes, not a persistent Node VM.",
              "trials": []}
    for name in ("server", "rsemu", "config"):
        report.setdefault("sha256", {})[name] = hashlib.sha256(
            getattr(args, name).read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix="rsemu-jit-bench-") as temporary:
        for repetition in range(args.repetitions):
            order = ("cranelift", "direct") if repetition % 2 == 0 else ("direct", "cranelift")
            for backend in order:
                directory = Path(temporary) / f"{repetition}-{backend}"
                directory.mkdir()
                print(f"repetition {repetition + 1}: {backend}", file=sys.stderr)
                report["trials"].append(trial(args, backend, repetition, directory))
    groups = {}
    for run in report["trials"]:
        groups.setdefault((run["backend"], "startup", "cold"), []).append(run["startup_seconds"])
        for sample in run["measurements"]:
            groups.setdefault((run["backend"], sample["workload"], sample["mode"]), []).append(sample["seconds"])
            if sample["hot_seconds"] is not None:
                groups.setdefault((run["backend"], sample["workload"] + "_hot", sample["mode"]), []).append(sample["hot_seconds"])
    report["medians"] = [{"backend": b, "workload": w, "mode": m,
                           "seconds": statistics.median(values), "samples": len(values)}
                          for (b, w, m), values in groups.items()]
    rendered = json.dumps(report, indent=2, default=str) + "\n"
    if args.output:
        args.output.write_text(rendered)
    sys.stdout.write(rendered)


if __name__ == "__main__":
    try:
        main()
    except (Exception, KeyboardInterrupt) as error:
        print(f"jit-bench: {error}", file=sys.stderr)
        sys.exit(1)
