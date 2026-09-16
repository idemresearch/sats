"""Bounded terminal smoke/regressions; invoked by human_review.rs, no pip packages."""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import subprocess
import sys
import termios
import time

BINARY, ROOT = sys.argv[1], Path(sys.argv[2])
PASSWORD = "integration-test-pw"
ADDRESS = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n"
ENV = dict(os.environ, SATS_DIR=str(ROOT), SATS_PASSWORD=PASSWORD, NO_COLOR="1")


def cli(*args, ok=True, **kwargs):
    result = subprocess.run([BINARY, *args], env=ENV, stdin=subprocess.DEVNULL,
                            capture_output=True, timeout=20, **kwargs)
    assert result.returncode == 0 if ok else result.returncode != 0, result.stderr.decode()
    return result


def cli_json(*args):
    return json.loads(cli("--json", *args).stdout)


def read_record(request_id):
    return json.loads(record_path(request_id).read_text())


def record_path(request_id):
    return ROOT / "signet/agent-requests/claude" / (request_id + ".json")


def write_record(record):
    record_path(record["id"]).write_text(json.dumps(record))


def grant():
    return cli_json("agent", "grant", "claude", "--budget", "50000")["token"]


class MCP:
    def __init__(self, token):
        self.proc = subprocess.Popen([BINARY, "agent", "serve", "claude"],
                                     env=dict(ENV, SATS_AGENT_TOKEN=token),
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE)
        self.sequence = 0
        self.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                "clientInfo": {"name": "human-review-test", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, data):
        self.proc.stdin.write((json.dumps(data) + "\n").encode())
        self.proc.stdin.flush()

    def rpc(self, method, params):
        self.sequence += 1
        self.send({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        assert select.select([self.proc.stdout], [], [], 10)[0], "MCP response timeout"
        response = json.loads(self.proc.stdout.readline())
        assert response["id"] == self.sequence, response
        return response["result"]

    def call(self, tool, args):
        return self.rpc("tools/call", {"name": tool, "arguments": args})["structuredContent"]

    def file(self, key, amount=4500):
        result = self.call("request_send", {"address": ADDRESS, "amount_sat": amount,
                                           "idempotency_key": key})
        assert result["status"] == "pending_approval", result
        return result["request_id"]

    def check(self, request_id):
        return self.call("check_request", {"request_id": request_id})

    def close(self):
        self.proc.terminate()
        self.proc.communicate(timeout=10)


TERMINALS = []


class Terminal:
    def __init__(self, *, password=PASSWORD, args=()):
        self.master, slave = pty.openpty()
        env = dict(ENV)
        if password is None:
            env.pop("SATS_PASSWORD", None)
        else:
            env["SATS_PASSWORD"] = password

        def terminal_session():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.proc = subprocess.Popen([BINARY, "--json", "agent", "approve", *args],
                                     env=env, stdin=slave, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, preexec_fn=terminal_session)
        TERMINALS.append(self)
        os.close(slave)
        self.buffers = {self.master: b"", self.proc.stdout.fileno(): b"",
                        self.proc.stderr.fileno(): b""}
        self.live = set(self.buffers)

    def drain(self, timeout=0.1):
        for fd in select.select(list(self.live), [], [], timeout)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if not data:
                self.live.remove(fd)
            self.buffers[fd] += data

    def transcript(self):
        return b"".join(self.buffers.values()).decode(errors="replace")

    def wait_for(self, text, count=1):
        until = time.monotonic() + 15
        while self.transcript().count(text) < count:
            self.drain()
            assert time.monotonic() < until, (text, self.transcript())
            assert self.proc.poll() is None or self.live, (text, self.transcript())
        return self.transcript()

    def send(self, text):
        os.write(self.master, text.encode())

    def finish(self, ok=True):
        until = time.monotonic() + 20
        while self.proc.poll() is None or self.live:
            self.drain()
            if time.monotonic() >= until:
                self.proc.kill()
                raise AssertionError("terminal timeout: " + self.transcript())
        result = self.proc.wait()
        os.close(self.master)
        assert (result == 0) == ok, self.transcript()
        return self.buffers[self.proc.stdout.fileno()].decode()

    def choose(self, request_id):
        text = self.wait_for("q cancel: ")
        row = next(line for line in text.splitlines() if f"[{request_id}]" in line)
        number = re.match(r"(\d+)\.", row).group(1)
        self.send(number + "\n")


def unchanged(request_id, status="pending_approval"):
    assert read_record(request_id)["status"] == status
    saved = ROOT / "signet/transactions"
    assert not saved.exists() or not list(saved.glob("*.json"))
    policy = json.loads((ROOT / "signet/grants/claude.json").read_text())
    assert policy["spent_sat"] == 0 and policy.get("reservations", []) == [], policy


mcp = MCP(grant())
try:
    # Empty, refresh, bounded wait, and cancellation remain provider-free even
    # when provider capabilities are ambiguous. Explicit ID required for pipes.
    config = ROOT / "config.toml"
    original_config = config.read_text()
    config.write_text(original_config + '\n[providers.second]\ndriver="esplora"\nnetwork="signet"\nurl="http://127.0.0.1:1"\n')
    nonterminal = cli("agent", "approve", "--yes", ok=False)
    assert b"explicit request id" in nonterminal.stderr
    empty = Terminal()
    empty.wait_for("No requests available for approval.")
    empty.send("r\n")
    empty.wait_for("q cancel: ", 2)
    empty.send("w\n")
    empty.wait_for("q cancel: ", 3)
    empty.send("q\n")
    assert json.loads(empty.finish()) == {"status": "cancelled"}
    config.write_text(original_config)

    request_id = mcp.file("only-filing")
    first_snapshot = read_record(request_id)

    # One row never auto-selects or authorizes, even --yes. Selection must
    # separately confirm; choosing no leaves no reservation or transaction.
    review = Terminal(args=("--yes",))
    review.choose(request_id)
    review.wait_for("Approve and sign? [y/N]")
    unchanged(request_id)
    for field in ["Wallet", "Network", "signet", ADDRESS, "Amount", "Fee", "Total"]:
        assert field in review.transcript(), review.transcript()
    assert str((ROOT / "signet/wallet.sqlite").resolve()) in review.transcript()
    review.send("n\n")
    review.finish()
    unchanged(request_id)

    # The displayed snapshot binds the number even if a newer row arrives.
    queue = Terminal()
    queue.wait_for("q cancel: ")
    second_id = mcp.file("later-filing", 5500)
    newer = read_record(second_id)
    newer["created_at"] += 100
    write_record(newer)
    queue.send("1\n")
    queue.wait_for("Approve and sign?")
    assert f"Approve    {request_id}" in queue.transcript(), queue.transcript()
    queue.send("n\n")
    queue.finish()
    unchanged(request_id)
    cli("agent", "dismiss", second_id)

    # An explicit refresh displays many rows, then cancel selects nothing.
    third_id = mcp.file("third-filing", 6000)
    many = Terminal()
    text = many.wait_for("q cancel: ")
    assert request_id in text and third_id in text
    many.send("q\n")
    many.finish()
    cli("agent", "dismiss", third_id)

    # Changed and missing selected records fail closed instead of choosing
    # another queue row or silently reviewing modified payment metadata.
    for mutation in ["changed", "disappeared"]:
        stale = Terminal()
        stale.wait_for("q cancel: ")
        if mutation == "changed":
            changed = dict(first_snapshot, amount_sat=4501)
            write_record(changed)
        else:
            record_path(request_id).unlink()
        stale.send("1\n")
        stale.finish(ok=False)
        assert mutation in stale.transcript(), stale.transcript()
        write_record(first_snapshot)
        unchanged(request_id)

    # Wrong password never draws budget. The next attempt uses the same filing.
    wrong = Terminal(password="incorrect-password")
    wrong.choose(request_id)
    wrong.wait_for("Approve and sign?")
    wrong.send("y\n")
    wrong.finish(ok=False)
    unchanged(request_id)

    # Existing execution claim prevents a competing approval while review is
    # on-screen; cancelling releases the claim without signing.
    holder = Terminal()
    holder.choose(request_id)
    holder.wait_for("Approve and sign?")
    competing = cli("agent", "approve", request_id, "--yes", ok=False)
    assert b"signing right now" in competing.stderr
    holder.send("n\n")
    holder.finish()
    unchanged(request_id)

    # Recovery rows stay visible but never become selectable. Failed is the
    # only retryable failure because the signer was never invoked.
    for status, extra, selectable in [
        ("failed", {"message": "fixture pre-sign failure", "at": 1}, True),
        ("broadcast_pending", {"txid": "ab" * 32, "fee_sat": 200, "at": 1}, False),
        ("unresolved", {"message": "fixture uncertainty", "at": 1}, False),
        ("signing", {"approved_at": 1, "fee_sat": 200}, False),
    ]:
        write_record(dict(first_snapshot, status=status, **extra))
        recovery = Terminal()
        text = recovery.wait_for("q cancel: ")
        assert ("1. claude" in text) == selectable, text
        assert "fixture pre-sign failure" in text if selectable else "Attention:" in text
        if status == "broadcast_pending":
            assert "sats tx broadcast " + "ab" * 32 in text
        if status in ("unresolved", "signing"):
            assert "never approve again" in text
        recovery.send("q\n")
        recovery.finish()
        listed = cli_json("agent", "requests")
        assert request_id in [r["id"] for r in listed]
        write_record(first_snapshot)

    # One real filing -> menu -> separate confirmation -> actual hidden password
    # -> check_request:sent. There is exactly one signature and one ledger draw.
    happy = Terminal(password=None)
    happy.choose(request_id)
    happy.wait_for("Approve and sign?")
    assert "password:" not in happy.transcript()
    happy.send("y\n")
    happy.wait_for("password:")
    # The library prints its prompt just before disabling echo. Wait for the
    # terminal mode change instead of racing the prompt with instant input.
    hidden_deadline = time.monotonic() + 5
    while termios.tcgetattr(happy.master)[3] & termios.ECHO:
        assert time.monotonic() < hidden_deadline, "password echo stayed enabled"
        time.sleep(0.01)
    happy.send(PASSWORD + "\n")
    approved = json.loads(happy.finish())
    assert approved["status"] == "sent" and approved["id"] == request_id, approved
    assert PASSWORD not in happy.transcript(), "password was echoed"
    observed = mcp.check(request_id)
    assert observed["status"] == "sent" and observed["txid"] == approved["txid"], observed
    events = [json.loads(line) for line in (ROOT / "signet/events/log.jsonl").read_text().splitlines()]
    kinds = [e["event"] for e in events if e.get("request_id") == request_id]
    for kind in ["request_received", "approved", "reserved", "signed", "broadcast"]:
        assert kinds.count(kind) == 1, kinds
    policy = json.loads((ROOT / "signet/grants/claude.json").read_text())
    assert len(policy["reservations"]) == 1
    assert policy["reservations"][0]["request_id"] == request_id
    assert policy["spent_sat"] == approved["total_sat"]
    assert len(list((ROOT / "signet/transactions").glob("*.json"))) == 1
    cli("agent", "approve", request_id, "--yes", ok=False)

    # Revocation and re-issue between selection and action stay hard boundaries
    # and are checked before provider resolution or any password request.
    for replacement in [False, True]:
        pending_id = mcp.file("replace" if replacement else "revoke")
        stale_grant = Terminal()
        stale_grant.wait_for("q cancel: ")
        cli("agent", "revoke", "claude")
        if replacement:
            next_token = grant()
        stale_grant.choose(pending_id)
        denied = json.loads(stale_grant.finish())
        assert denied["status"] == "denied", denied
        assert "password:" not in stale_grant.transcript()
        if not replacement:
            next_token = grant()
        mcp.close()
        mcp = MCP(next_token)
    print("terminal approval scenarios passed")
finally:
    for terminal in TERMINALS:
        if terminal.proc.poll() is None:
            terminal.proc.kill()
            terminal.proc.wait(timeout=5)
    mcp.close()
