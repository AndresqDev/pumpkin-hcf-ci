#!/usr/bin/env python3
"""B1 sampler: /tps, /tick query percentiles and /proc CPU/RSS.

Uso:
  PUMPKIN_PID=123 VIAPROXY_PID=456 sampler_b1.py --out b1.csv --interval 2
"""
import argparse
import os
import re
import socket
import struct
import subprocess
import sys
import time

CLK_TCK = os.sysconf("SC_CLK_TCK")
START = time.time()
PREV = {}


def _packet(rid, typ, body: bytes) -> bytes:
    return struct.pack("<iii", 10 + len(body), rid, typ) + body + b"\x00\x00"


def _read_packet(sock):
    raw = sock.recv(4)
    if len(raw) < 4:
        return None
    (length,) = struct.unpack("<i", raw)
    data = b""
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            break
        data += chunk
    return data


def rcon(cmd, host, port, password, timeout=4.0):
    try:
        s = socket.create_connection((host, port), timeout=timeout)
        s.settimeout(1.5)
        s.sendall(_packet(1, 3, password.encode()))
        resp = _read_packet(s)
        if not resp or resp[4:8] != struct.pack("<i", 2):
            s.close()
            return None
        s.sendall(_packet(2, 2, cmd.encode()))
        out = []
        while True:
            try:
                resp = _read_packet(s)
            except socket.timeout:
                break
            if not resp:
                break
            body = resp[8:-2]
            if not body:
                break
            out.append(body.decode("utf-8", "replace"))
        s.close()
        return "\n".join(out).strip()
    except Exception:
        return None


def proc_sample(pid):
    if not pid:
        return None, None
    try:
        with open(f"/proc/{pid}/stat", "r", encoding="utf-8") as f:
            data = f.read()
        rp = data.rfind(")")
        fields = data[rp + 2:].split()
        ticks = int(fields[11]) + int(fields[12])
        rss_kb = int(fields[21]) * (os.sysconf("SC_PAGE_SIZE") // 1024)
        now = time.time()
        prev_ticks, prev_ts = PREV.get(pid, (ticks, now))
        PREV[pid] = (ticks, now)
        dt = max(now - prev_ts, 0.001)
        cpu = (ticks - prev_ticks) / CLK_TCK / dt * 100.0
        return round(cpu, 1), rss_kb
    except Exception:
        return None, None


def loadavg():
    try:
        return float(open("/proc/loadavg").read().split()[0])
    except Exception:
        return None


def memavail_kb():
    try:
        for line in open("/proc/meminfo"):
            if line.startswith("MemAvailable:"):
                return int(line.split()[1])
    except Exception:
        pass
    return None


def established(port):
    try:
        out = subprocess.run(
            ["ss", "-Htn", "state", "established", f"( sport = :{port} )"],
            capture_output=True, text=True, timeout=3,
        ).stdout
        return len([ln for ln in out.splitlines() if ln.strip()])
    except Exception:
        return None


def parse_online(list_out):
    if not list_out:
        return None
    m = re.search(r"There are (\d+) of a max", list_out)
    if m:
        return int(m.group(1))
    m = re.search(r"(\d+)\s+players?\s+online", list_out, re.I)
    if m:
        return int(m.group(1))
    m = re.search(r":\s*(.+)$", list_out, re.S)
    if m:
        names = [n.strip() for n in m.group(1).split(",") if n.strip()]
        return len(names)
    return None


def parse_tps(raw):
    if not raw:
        return None, None
    m = re.search(r"TPS:\s*([\d.]+)\s*MSPT:\s*([\d.]+)", raw)
    if not m:
        return None, None
    return float(m.group(1)), float(m.group(2))


def parse_percentiles(raw):
    if not raw:
        return None, None, None, None
    vals = {}
    for name in ("P50", "P95", "P99"):
        m = re.search(rf"{name}:\s*([\d.]+)\s*ms", raw, re.I)
        if m:
            vals[name] = float(m.group(1))
    m = re.search(r"(?:Sample|sample|tick|Ticks):\s*(\d+)", raw)
    return vals.get("P50"), vals.get("P95"), vals.get("P99"), (int(m.group(1)) if m else None)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="b1.csv")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=25575)
    ap.add_argument("--password", default="lazarus-test")
    ap.add_argument("--interval", type=float, default=2.0)
    args = ap.parse_args()

    pumpkin_pid = int(os.environ.get("PUMPKIN_PID") or 0) or None
    viaproxy_pid = int(os.environ.get("VIAPROXY_PID") or 0) or None

    new_file = not os.path.exists(args.out)
    fh = open(args.out, "a", buffering=1, encoding="utf-8")
    if new_file:
        fh.write(
            "ts,elapsed,tps,mspt_avg,p50,p95,p99,tick_sample,players,"
            "pumpkin_cpu,viaproxy_cpu,pumpkin_rss_kb,viaproxy_rss_kb,"
            "clients_25565,backend_25566,load1,memavail_kb\n"
        )

    def shutdown(*_):
        try:
            fh.flush()
            fh.close()
        finally:
            sys.exit(0)

    import signal
    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    print(
        f"sampler_b1: out={args.out} rcon={args.host}:{args.port} "
        f"pumpkin_pid={pumpkin_pid} viaproxy_pid={viaproxy_pid}",
        flush=True,
    )

    while True:
        ts = int(time.time())
        elapsed = int(ts - START)
        tps, mspt = parse_tps(rcon("tps", args.host, args.port, args.password))
        p50, p95, p99, sample = parse_percentiles(
            rcon("tick query", args.host, args.port, args.password)
        )
        players = parse_online(rcon("list", args.host, args.port, args.password))
        p_cpu, p_rss = proc_sample(pumpkin_pid)
        v_cpu, v_rss = proc_sample(viaproxy_pid)
        c65 = established(25565)
        b66 = established(25566)
        row = [
            ts, elapsed,
            "" if tps is None else tps,
            "" if mspt is None else mspt,
            "" if p50 is None else p50,
            "" if p95 is None else p95,
            "" if p99 is None else p99,
            "" if sample is None else sample,
            "" if players is None else players,
            "" if p_cpu is None else p_cpu,
            "" if v_cpu is None else v_cpu,
            "" if p_rss is None else p_rss,
            "" if v_rss is None else v_rss,
            "" if c65 is None else c65,
            "" if b66 is None else b66,
            "" if loadavg() is None else loadavg(),
            "" if memavail_kb() is None else memavail_kb(),
        ]
        fh.write(",".join(str(x) for x in row) + "\n")
        print(
            f"SAMPLE t={elapsed}s tps={tps} mspt={mspt} "
            f"p50={p50} p95={p95} p99={p99} sample={sample} players={players} "
            f"cpu[pumpkin={p_cpu}% viaproxy={v_cpu}%] "
            f"rss[pumpkin={p_rss}KB viaproxy={v_rss}KB] "
            f"conn[{c65}/{b66}]",
            flush=True,
        )
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
