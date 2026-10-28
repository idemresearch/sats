"""Check human Alkanes review before password input, using disposable state."""
import fcntl
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import termios
import time

binary, root = sys.argv[1], Path(sys.argv[2])
request_id = sys.argv[3] if len(sys.argv) > 3 else None
RECIPIENT = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n"

for json_mode, yes, cancel in [(False, False, False), (True, False, False),
                               (True, True, False), (True, False, True)]:
    master, slave = pty.openpty()
    env = dict(os.environ, SATS_DIR=str(root), NO_COLOR="1")
    env.pop("SATS_PASSWORD", None)

    def session():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    args = ([binary, "agent", "approve", request_id] if request_id else
            [binary, "alkanes", "send", "32:0", "1000", RECIPIENT])
    confirmation = "Approve and sign?" if request_id else "Sign?"
    if json_mode:
        args.append("--json")
    if yes:
        args.append("--yes")
    proc = subprocess.Popen(args, env=env, stdin=slave, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, preexec_fn=session)
    os.close(slave)
    buffers = {master: b"", proc.stdout.fileno(): b"", proc.stderr.fileno(): b""}
    live = set(buffers)

    def drain(timeout=0.1):
        for fd in select.select(list(live), [], [], timeout)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if not data:
                live.discard(fd)
            buffers[fd] += data

    def transcript():
        return b"".join(buffers.values()).decode(errors="replace")

    def wait_for(text):
        until = time.monotonic() + 15
        while text not in transcript():
            drain()
            assert time.monotonic() < until, transcript()
            assert proc.poll() is None or live, transcript()
        drain(0)

    try:
        if not yes:
            wait_for(confirmation)
            assert "password:" not in transcript(), transcript()
            if not cancel:
                os.write(master, b"y\n")
        if not cancel:
            wait_for("password:")
        review_fd = proc.stderr.fileno() if json_mode else proc.stdout.fileno()
        review = buffers[review_fd].decode(errors="replace")
        fields = (["Wallet", "Network", "signet", "Approve", request_id, "Asset transfer",
                   "alkanes:32:0", "1000 base units", "Returned assets", "32:0: 4000",
                   "2:0: 50", "546", "Fee", "Total"] if request_id else
                  ["Wallet", "Network", "signet", "Send", "1000 of 32:0", "To", RECIPIENT,
                   "Kept", "Postage", "Fee", "Total", "leave the wallet"])
        for text in fields:
            assert text in review, (text, transcript())
        if json_mode:
            assert buffers[proc.stdout.fileno()] == b"", transcript()
            if not yes:
                assert confirmation in review, review
        # The review is already visible while unlock is waiting. An incorrect
        # password proves none of these review paths signs or broadcasts.
        os.write(master, b"n\n" if cancel else b"wrong-password\n")
        until = time.monotonic() + 20
        while proc.poll() is None or live:
            drain()
            assert time.monotonic() < until, transcript()
        if cancel:
            assert proc.returncode == 0, transcript()
            assert "aborted" in buffers[proc.stderr.fileno()].decode(), transcript()
            assert "password:" not in transcript(), transcript()
        else:
            assert proc.returncode != 0, transcript()
            assert "wrong password" in transcript(), transcript()
        if json_mode:
            assert buffers[proc.stdout.fileno()] == b"", transcript()
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait(timeout=5)
        os.close(master)
        proc.stdout.close()
        proc.stderr.close()

assert not (root / "mockdata/broadcasts.log").exists()
transactions = root / "signet/transactions"
assert not transactions.exists() or not list(transactions.glob("*.json"))
print("Alkanes review precedes confirmation/password in text, JSON and --yes modes")
