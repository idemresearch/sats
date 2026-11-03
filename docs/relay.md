# Relay

A relay lets an agent that can't run sats next to your wallet, such as
ChatGPT on a phone (which reaches MCP servers from OpenAI's servers), file
requests to it. There are three parts:

- **sats agent:** the agent's side. It files requests and observes them over
  MCP.
- **sats relay:** an intermediary with a public HTTPS address. It holds no
  wallet and no key, and it can't approve, sign, or broadcast.
- **sats human:** the device that holds the wallet. Today that's the `sats`
  CLI. It decides every request against its own grant, and you approve
  there.

```text
agent ──MCP over HTTPS──▶ relay ◀──signed sync── device (sats CLI) ──▶ chain
         files, observes         queues sealed        decides, you approve,
                                 filings, results     signs, broadcasts
```

## Run a relay

The relay is its own binary, `sats-relay`, published as a Docker image built
from the repository's `Dockerfile`:

```sh
docker build -t sats-relay .
docker run -p 8080:8080 -v sats-relay:/data sats-relay
```

Put TLS in front of it with a reverse proxy or your platform's HTTPS.
Configuration is by flag or environment variable:

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--listen` | `SATS_RELAY_LISTEN` | `127.0.0.1:8080` (image: `0.0.0.0:8080`) | Address to listen on |
| `--data-dir` | `SATS_RELAY_DATA` | `./sats-relay-data` (image: `/data`) | Mailbox database directory |
| `--filing-ttl` | `SATS_RELAY_FILING_TTL` | `7d` | How long a request waits for your device |
| `--result-ttl` | `SATS_RELAY_RESULT_TTL` | `30d` | How long a result stays observable |

`GET /healthz` answers `{"ok":true}`. The relay accepts any `Host` header:
every route except `/healthz` is authenticated by a signature, so a host
allowlist would add nothing but breakage behind proxies.

The image contains the relay binary and a small entrypoint. Started as
root, the entrypoint takes ownership of `/data`, since platforms often mount
volumes owned by root, and then drops to the unprivileged `sats` user before
the relay runs. The relay crate doesn't link the wallet, so it can't open
one, unseal a seed, or build a signer.

## Connect an agent

On the device that holds the wallet:

```sh
sats relay link https://relay.example.com
sats agent grant chatgpt --for 7d --max-tx 10k --relay
sats relay sync --watch
```

- `relay link` creates this device's relay key and records the relay. The
  key is used only to talk to the relay, never to sign transactions. It's
  stored per wallet and network in `relay.json`, with owner-only permissions.
  Linking again keeps the key; `--new-key` replaces it, which retires every
  agent URL minted with the old one.
- `agent grant --relay` creates an ordinary grant and prints two things:
  the relay's MCP URL, `https://relay.example.com/mcp`, and a connection
  code, `https://relay.example.com/mcp/<capability>`. The code carries the
  grant's token, and sats shows it only once. With `--json` they are
  `relay_mcp_url` and `relay_url`.
- `relay sync` fetches what agents filed and decides each filing here. It
  then posts each request's state back so the agent can observe it.
  `--watch` repeats every `--every` seconds (default 10). Approve as usual
  with `sats agent approve`.

### Sign in with OAuth

Add `https://relay.example.com/mcp` to your agent as a remote MCP server
that signs in with OAuth (in ChatGPT, a custom MCP server with OAuth). The
agent registers itself with the relay and opens a sign-in page. Paste the
connection code there, as the bare code or the whole line. That paste is
your consent. The page names the app asking, from its redirect address.

- The agent gets an access token (one hour) and a refresh token (replaced
  on every use). Neither outlives the grant's expiry.
- Each token stands for the connection code you pasted: every call checks
  its signature and expiry again, and your device still decides every
  filing against its grant.
- To cut an agent off, revoke or re-issue the grant on your device as
  usual. Its filings are refused at your next sync, and its tokens stop
  working when the grant expires.
- The relay supports dynamic client registration and PKCE (`S256`) for
  public clients. Redirect addresses must be HTTPS, or HTTP to loopback.

### Without OAuth

An agent that can't sign in can use the connection code itself as its MCP
URL, with no authentication. The URL is then the credential: it may be kept
in the agent's settings and in logs that record paths.

## What the agent can do

The relay serves `get_grant`, `request_send`, `request_wrap_frbtc`, and
`check_request`. They have the same names, parameters, and result shapes as
on a local server ([MCP](mcp.md)), with these differences:

- **Filing queues.** A new filing returns `pending_approval` straight away.
  Your device decides it at its next sync: as `pending_approval` for you to
  review, `denied` at a grant boundary, or `status: "error"` if it refuses
  the filing (fx `no_grant` after you revoke).
- **`check_request` returns what your device posted.** It's exactly what a
  local `check_request` would return, including `sent` and its `txid`.
  `sats agent approve` and `sats agent dismiss` post the new state to the
  relay as soon as they finish; if the relay can't be reached they warn,
  and the next `sats relay sync` posts it. Until the device has synced a
  new filing, it reads `pending_approval`.
- **Idempotency holds across the relay.** The relay derives the same
  request id as your device, so a retried key returns the same request. A
  reused key with a different request gets `idempotency_key_conflict`.
- **`get_grant` shows only what the URL carries:** agent, network, and
  expiry, with `relayed: true`. Your device holds the amount cap,
  allowlist, and mode, and enforces them.
- **Wallet reads aren't relayed.** `get_balance`, `get_receive_address`, and
  the Alkanes reads stay local-only, so the relay never sees your
  addresses or balances.
- **Mailbox limits.** At most 64 requests can wait for one device; past
  that, filing returns `relay_mailbox_full`.

## Revocation and expiry

`sats agent revoke` takes effect at your device's next sync. Filings under
the old grant are refused there and the agent observes `no_grant`. The URL
stops working at the relay once its expiry passes. Re-issue the grant to
rotate the URL.

## Security

See [Security](security.md#the-relay) for the protocol and what a
compromised relay can and can't do.
