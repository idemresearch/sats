"""Enter alone at the review of an asset labelled like frBTC must not sign."""
import fcntl
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import termios
import time

binary, root, asset, recipient = sys.argv[1], Path(sys.argv[2]), sys.argv[3], sys.argv[4]
master, slave = pty.openpty()
env = dict(os.environ, SATS_DIR=str(root), NO_COLOR="1")
env.pop("SATS_PASSWORD", None)


def session():
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)


proc = subprocess.Popen([binary, "alkanes", "send", asset, "7", recipient, "--fee-rate", "2"],
                        env=env, stdin=slave, stdout=slave, stderr=slave, preexec_fn=session)
os.close(slave)
seen = b""


def read(until):
    global seen
    deadline = time.monotonic() + 20
    while until not in seen:
        assert time.monotonic() < deadline, seen.decode(errors="replace")
        if select.select([master], [], [], 0.1)[0]:
            try:
                data = os.read(master, 65536)
            except OSError:
                data = b""
            assert data, seen.decode(errors="replace")
            seen += data


try:
    read(b"Sign? [y/N]")
    text = seen.decode(errors="replace")
    assert f"{asset} calls itself frBTC, but frBTC is 32:0" in text, text
    os.write(master, b"\r")
    read(b"aborted")
    assert proc.wait(timeout=20) == 0
    assert b"password:" not in seen, seen.decode(errors="replace")
finally:
    if proc.poll() is None:
        proc.kill()
    os.close(master)

assert not (root / "mockdata/broadcasts.log").exists()
print("Enter at an impostor's review cancels without unlocking")
