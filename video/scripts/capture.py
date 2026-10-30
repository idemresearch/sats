"""Record the launch video's terminal scenes from the real sats binary.

Every line the video's terminal shows comes from here: a debug build of
sats, driven on a pseudo-terminal exactly as a person would use it, against
the file-driven mock chain the integration tests use. Nothing is typed into
the recordings by hand. The only edits are redactions: the disposable home
directory is shown as `~`, and the password is recorded as asterisks.

Writes asciicast v2 recordings (`asciinema play captures/<name>.cast`) and
the agent's MCP exchanges as JSON into video/captures/.

Usage: python3 video/scripts/capture.py   (from anywhere; needs cargo)
"""

import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "video" / "captures"
COLUMNS, ROWS = 110, 24
# Short, like a real one: the video plays a keystroke per character.
PASSWORD = "signet-demo"
# BIP-39 test vectors, so every capture derives the same wallets.
WALLET_PHRASE = " ".join(["abandon"] * 11 + ["about"])
RECIPIENT_PHRASE = " ".join(["zoo"] * 11 + ["wrong"])
# Confirmed outputs: two 546-sat carriers, then plain BTC for fees.
FUNDING = [546, 546, 98_000, 60_000]
# frBTC's public contract facts, as the Subfrost indexer reports them: the
# fixtures crates/sats-cli/tests/mcp.rs wraps against.
FRBTC_BYTECODE = {"32:0": "0x0061736d"}
FRBTC_SIGNER = "0a241a22512030685674457c3eaa6d2904a1340ab80296625e8c7313f54c6ae7c51318f5860710e1fc02"
FRBTC_WRAP = ("0a5e0a0d0a060a0208201200120308864e122b0a262f7365656e2fffdbcfe2faaab58dcfe6363d4cad67bf68cc"
              "ae88d3e55e0a685a440e35a85f7412010112200a0c2f746f74616c737570706c7912107acbb22f01000000000000"
              "00000000001093880f")


def run(args, env, stdin=None, cwd=None):
    result = subprocess.run(args, env=env, input=stdin, cwd=cwd,
                            capture_output=True, text=True)
    if result.returncode != 0:
        sys.exit(f"{' '.join(map(str, args))} failed:\n{result.stdout}{result.stderr}")
    return result.stdout


def mock_wallet(sats, sats_dir, phrase, env):
    """Restore `phrase` into `sats_dir`, on the mock chain."""
    mockdata = sats_dir / "mockdata"
    mockdata.mkdir(parents=True)
    (mockdata / "alkanes-outpoints.json").write_text("{}")
    (sats_dir / "config.toml").write_text(
        'network = "signet"\n\n[signet]\nchain = "esplora"\n\n'
        f'[signet.esplora]\nurl = "file://{mockdata}"\n')
    setup = dict(env, SATS_DIR=str(sats_dir), SATS_PASSWORD=PASSWORD, NO_COLOR="1")
    run([sats, "init", "--restore"], setup, stdin=phrase + "\n")
    return mockdata, setup


class Recording:
    """One command on a pseudo-terminal, recorded as asciicast v2 events."""

    def __init__(self, sats, args, env):
        self.command = "sats " + " ".join(args)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.proc = subprocess.Popen([sats, *args], env=env, stdin=slave, stdout=slave,
                                     stderr=slave, preexec_fn=controlling_terminal)
        os.close(slave)
        self.master = master
        self.decoder = codecs.getincrementaldecoder("utf-8")()
        self.start = time.monotonic()
        self.events = []
        self.seen = ""

    def now(self):
        return round(time.monotonic() - self.start, 3)

    def pump(self, timeout=0.05):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError:  # EIO: the program closed the terminal
                data = b""
            if not data:
                return False
            text = self.decoder.decode(data)
            if text:
                self.events.append([self.now(), "o", text])
                self.seen += text
        return True

    def expect(self, prompt, reply, secret=False):
        """Wait for `prompt`, then answer as a person would: a pause, keys, Enter."""
        deadline = time.monotonic() + 30
        while prompt not in self.seen:
            if not self.pump() or time.monotonic() > deadline:
                sys.exit(f"{self.command}: never asked {prompt!r}:\n{self.seen}")
        self.seen = ""
        time.sleep(0.4)
        self.events.append([self.now(), "i", ("*" * len(reply) if secret else reply) + "\r"])
        os.write(self.master, reply.encode() + b"\r")

    def finish(self, home):
        deadline = time.monotonic() + 30
        while self.pump():
            if time.monotonic() > deadline:
                sys.exit(f"{self.command}: still running:\n{self.seen}")
        if self.proc.wait(timeout=30) != 0:
            sys.exit(f"{self.command} exited {self.proc.returncode}:\n{self.seen}")
        os.close(self.master)
        events = []
        for event in self.events:
            # Join output split across reads, so a path can't straddle two events.
            if events and event[1] == "o" and events[-1][1] == "o" and event[0] - events[-1][0] < 0.02:
                events[-1][2] += event[2]
            else:
                events.append(event)
        for event in events:
            event[2] = event[2].replace(str(home), "~")
            assert str(home) not in event[2] and PASSWORD not in event[2], event
        return events


def write_cast(name, recording, events, version, commit):
    header = {
        "version": 2, "width": COLUMNS, "height": ROWS,
        "timestamp": int(time.time()), "command": recording.command,
        "title": f"sats {version} ({commit})",
        "env": {"TERM": "xterm-256color", "SHELL": "/bin/sh"},
    }
    with open(OUT / f"{name}.cast", "w") as cast:
        cast.write(json.dumps(header) + "\n")
        for event in events:
            cast.write(json.dumps(event, ensure_ascii=False) + "\n")
    print(f"  {name}.cast  {recording.command}")


class Mcp:
    """The agent's side: JSON-RPC over the served process's stdio."""

    def __init__(self, sats, env, token, agent="claude"):
        self.proc = subprocess.Popen([sats, "agent", "serve", agent],
                                     env=dict(env, SATS_AGENT_TOKEN=token),
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True)
        self.id = 0
        self.call("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                 "clientInfo": {"name": "sats-video", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def call(self, method, params):
        self.id += 1
        self.send({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        while not (line := self.proc.stdout.readline()).strip():
            pass
        return json.loads(line)["result"]

    def tool(self, name, arguments):
        result = self.call("tools/call", {"name": name, "arguments": arguments})
        return {"tool": name, "arguments": arguments, "result": result["structuredContent"]}

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)


def funded_wallet(sats, cargo, sats_dir, env):
    """The demo wallet: restored, funded, with alkanes on its two carriers."""
    mockdata, setup = mock_wallet(sats, sats_dir, WALLET_PHRASE, env)
    funded = run([cargo, "test", "-p", "sats-cli", "--locked", "--quiet", "--test", "video",
                  "--", "--ignored", "--nocapture"],
                 dict(os.environ, SATS_VIDEO_DIR=str(sats_dir),
                      SATS_VIDEO_FUND=",".join(map(str, FUNDING))), cwd=ROOT)
    outpoints = re.findall(r"^funded (\S+)$", funded, re.M)
    assert len(outpoints) == len(FUNDING), funded
    # The indexer's view: alkanes on the two carriers, nothing on the rest.
    (mockdata / "alkanes-outpoints.json").write_text(json.dumps({
        outpoints[0]: {"32:0": "250000", "2:0": "1200"},
        outpoints[1]: {"32:0": "150000"},
    }))
    return mockdata, setup


def approve(sats, request_id, terminal, home):
    """The human's side of a request: review, confirm, unlock."""
    recording = Recording(sats, ["agent", "approve", request_id], terminal)
    recording.expect("Approve and sign? [Y/n]", "y")
    recording.expect("password:", PASSWORD, secret=True)
    return recording, recording.finish(home)


def record_assistant(sats, cargo, work, env, bob, version, commit):
    """The AI launch: a person asks an assistant, which files; the person approves.

    The chat around these calls is the video's script; the calls, their
    results, and the approvals are recorded here.
    """
    home = Path(work) / "assistant-home"
    sats_dir = home / ".local" / "share" / "sats"
    mockdata, setup = funded_wallet(sats, cargo, sats_dir, env)
    (mockdata / "alkanes-bytecode.json").write_text(json.dumps(FRBTC_BYTECODE))
    (mockdata / "alkanes-signer.hex").write_text(FRBTC_SIGNER)
    (mockdata / "alkanes-wrap.hex").write_text(FRBTC_WRAP)
    terminal = dict(env, SATS_DIR=str(sats_dir), HOME=str(home))

    token = json.loads(run([sats, "--json", "agent", "grant", "assistant", "--for", "24h",
                            "--max-tx", "20k"], setup))["token"]
    mcp = Mcp(sats, terminal, token, agent="assistant")
    calls = []

    sent = mcp.tool("request_alkanes_send", {"id": "frbtc", "amount": "25000",
                                             "address": bob, "idempotency_key": "send-bob"})
    assert sent["result"]["status"] == "pending_approval", sent
    calls.append(sent)
    recording, events = approve(sats, sent["result"]["request_id"], terminal, home)
    write_cast("assistant-approve-send", recording, events, version, commit)
    calls.append(mcp.tool("check_request", {"request_id": sent["result"]["request_id"]}))
    assert calls[-1]["result"]["status"] == "sent", calls[-1]

    wrap = mcp.tool("request_wrap_frbtc", {"amount_sat": 10000, "idempotency_key": "wrap-10k"})
    assert wrap["result"]["status"] == "pending_approval", wrap
    calls.append(wrap)
    recording, events = approve(sats, wrap["result"]["request_id"], terminal, home)
    write_cast("assistant-approve-wrap", recording, events, version, commit)
    calls.append(mcp.tool("check_request", {"request_id": wrap["result"]["request_id"]}))
    assert calls[-1]["result"]["status"] == "sent", calls[-1]
    mcp.close()

    (OUT / "assistant-mcp.json").write_text(json.dumps({
        "sats": version, "commit": commit, "agent": "assistant", "calls": calls,
    }, indent=2, ensure_ascii=False) + "\n")
    print("  assistant-mcp.json  " + ", ".join(call["tool"] for call in calls))


def main():
    cargo = shutil.which("cargo") or sys.exit("capture needs cargo")
    # The debug build: release builds refuse the mock chain and read the
    # password only from a terminal, which the setup steps below can't use.
    run([cargo, "build", "-p", "sats-cli", "--locked", "--quiet"], os.environ, cwd=ROOT)
    sats = str(Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug" / "sats")
    version = run([sats, "--version"], os.environ).split()[-1]
    commit = run(["git", "rev-parse", "--short", "HEAD"], os.environ, cwd=ROOT).strip()
    OUT.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="sats-video-") as work:
        home = Path(work) / "home"
        # sats' default data directory on Linux, so the review shows a real path.
        sats_dir = home / ".local" / "share" / "sats"
        env = {key: value for key, value in os.environ.items()
               if key not in ("NO_COLOR", "SATS_PASSWORD", "SATS_DIR")}
        env.update(HOME=str(home), TERM="xterm-256color", LANG="C.UTF-8")

        _, recipient_env = mock_wallet(sats, Path(work) / "recipient", RECIPIENT_PHRASE, env)
        alice = run([sats, "receive"], recipient_env).split()[0]
        bob = run([sats, "receive"], recipient_env).split()[0]

        mockdata, setup = funded_wallet(sats, cargo, sats_dir, env)

        terminal = dict(env, SATS_DIR=str(sats_dir))
        print(f"sats {version} ({commit}), {COLUMNS}x{ROWS}:")

        balance = Recording(sats, ["alkanes", "balance"], terminal)
        write_cast("alkanes-balance", balance, balance.finish(home), version, commit)

        # By name: frbtc is 32:0, and nobody needs to remember that.
        send = Recording(sats, ["alkanes", "send", "frbtc", "40000", alice], terminal)
        send.expect("Sign? [Y/n]", "y")
        send.expect("password:", PASSWORD, secret=True)
        write_cast("alkanes-send", send, send.finish(home), version, commit)

        # The agent: a grant (its token is printed once), then a filed transfer.
        token = json.loads(run([sats, "--json", "agent", "grant", "claude", "--for", "24h",
                                "--max-tx", "10k"], setup))["token"]
        mcp = Mcp(sats, terminal, token)
        filed = mcp.tool("request_alkanes_send", {"id": "frbtc", "amount": "25000",
                                                  "address": bob, "idempotency_key": "pay-bob"})
        assert filed["result"]["status"] == "pending_approval", filed
        request_id = filed["result"]["request_id"]

        recording, events = approve(sats, request_id, terminal, home)
        write_cast("agent-approve", recording, events, version, commit)

        checked = mcp.tool("check_request", {"request_id": request_id})
        assert checked["result"]["status"] == "sent", checked
        mcp.close()
        (OUT / "agent-mcp.json").write_text(json.dumps({
            "sats": version, "commit": commit, "agent": "claude",
            "calls": [filed, checked],
        }, indent=2, ensure_ascii=False) + "\n")
        print("  agent-mcp.json  request_alkanes_send, check_request")

        record_assistant(sats, cargo, work, env, bob, version, commit)


if __name__ == "__main__":
    main()
