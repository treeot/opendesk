# Relay deployment

The relay is a single HTTP service: a WebSocket endpoint at `/ws` and a health
check at `/health` (it returns `{"status":"ok"}`). It does not terminate TLS
itself; the Compose setup below puts Caddy in front for automatic HTTPS. The
only state it keeps is which token owns each host ID (see
[Configuration](#configuration)).

## Requirements

- a server reachable by the clients (any VPS; amd64 or arm64);
- a DNS name pointing at it, such as `relay.example.com`; and
- ports 80 and 443 open (port 80 serves only the certificate challenge).

Clients need no open ports. Clients try direct UDP between themselves; the
relay fallback and all signaling share the one HTTPS port.

## Docker Compose (recommended)

The repo's `compose.yaml` runs the relay behind Caddy, which obtains and renews a
Let's Encrypt certificate.

```sh
git clone https://github.com/treeot/opendesk.git
cd opendesk
DOMAIN=relay.example.com docker compose up -d
```

Or copy `.env.example` to `.env`, set `DOMAIN`, and run `docker compose up -d`.
Check it with `curl https://relay.example.com/health`, then set each client's
relay to:

```text
wss://relay.example.com/ws
```

Update with `git pull && docker compose up -d --build`. Logs:
`docker compose logs -f relay` (set `RUST_LOG=debug` in `.env` for more).

## Prebuilt image

CI publishes `ghcr.io/treeot/opendesk-relay` (`latest` and version tags from
releases, `edge` from `main`). Run it behind your own TLS proxy:

```sh
docker run -d --restart unless-stopped -p 8080:8080 \
  -v relay_data:/data -e RELAY_STATE_FILE=/data/state.json \
  ghcr.io/treeot/opendesk-relay:latest
```

Forward `wss://` on 443 to the container's port 8080; WebSocket upgrades pass
through unchanged.

## Coolify

1. Create an application from this repository with build pack
   **Dockerfile**, **Base Directory** `/` (the repo root; the build needs the
   whole Cargo workspace), **Dockerfile Location** `/crates/relay/Dockerfile`,
   and **Ports Exposes** `8080`. Putting the Dockerfile path in Base Directory
   fails with `mkdir: can't create directory ... File exists`.
2. Set the domain (e.g. `relay.example.com`). Coolify provisions TLS and
   injects `PORT`, which the relay binds automatically (default 8080).
3. Deploy, then point clients at `wss://relay.example.com/ws`.

## Configuration

All settings are optional environment variables. The relay checks them at
startup: a value it can't use (not a whole number, zero, a malformed proxy
range) stops it with a message that names the variable, rather than quietly
falling back to the default. So does a state file that exists but can't be
read, since starting empty would overwrite the saved IDs at the next save.
Unknown `RELAY_*` variables are logged as a warning, which catches typos.

| Variable | Default | Meaning |
| --- | --- | --- |
| `RELAY_TRUSTED_PROXIES` | loopback, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7` | Comma-separated CIDRs of reverse proxies. `none` trusts nothing. |
| `RELAY_STATE_FILE` | unset | JSON file that keeps ID ownership across restarts. |
| `RELAY_ID_LEASE_SECS` | `2592000` (30 days) | How long an offline host keeps its ID. |
| `RELAY_UNPAIRED_ID_LEASE_SECS` | `86400` (1 day) | How long an ID that has never had a viewer connect keeps its owner. |
| `RELAY_MAX_ID_BINDINGS` | `100000` | Most IDs tracked at once. When full, the oldest offline IDs that never paired make room; if there are none, new IDs are refused. |
| `RELAY_NEW_ID_BURST` / `RELAY_NEW_ID_REFILL_SECS` | `20` / `3600` | New IDs one IP (or IPv6 /64) may claim per window. |
| `RELAY_MAX_RATE_ENTRIES` | `100000` | Most IPs tracked by each rate limiter. |
| `RELAY_IDLE_TIMEOUT_SECS` | `60` | Closes a paired connection after this long without traffic. |
| `RELAY_SESSION_CONTROL_BURST` / `RELAY_SESSION_CONTROL_PER_SEC` | `64` / `16` | Control messages allowed per paired connection. |
| `RELAY_SWEEP_SECS` | `60` | How often expired IDs and idle rate limits are removed and the state file is saved. |

**Client addresses behind a proxy.** Per-IP limits and logs use the client's
address. When a connection comes from a trusted proxy, the relay takes the
address from `X-Forwarded-For` (rightmost entry that isn't a trusted proxy),
or `X-Real-IP` if that header is missing. Headers from untrusted peers are
ignored. The defaults suit a relay on a private Docker network behind Caddy,
Traefik or Coolify. If the relay port is reachable directly from a network
you don't control, narrow `RELAY_TRUSTED_PROXIES` to your proxy's address.

**ID ownership.** The first host to register an ID owns it until it has been
offline for the lease period. Another device that tries to register the same
ID gets an `id_taken` error. The owner's token may replace its own live
registration, but if that keeps happening (more than 3 times in 10 minutes,
usually two copies of one machine) the newcomer gets `id_in_use`. Without `RELAY_STATE_FILE` this ownership is lost
when the relay restarts. The file stores a SHA-256 hash of each host token,
not the token itself. The Compose setup stores it in the `relay_data` volume.

## Clients

In the app's home screen, **Relay regions** takes one WebSocket URL per line,
including `/ws`; clients try them in order. The default development value is
`ws://127.0.0.1:8080/ws` (`cargo run -p relay`).

## STUN

ICE uses Google Public STUN at `stun:stun.l.google.com:19302` by default to
discover server-reflexive addresses. It's the default because it needs no
extra service and only takes part in candidate discovery. STUN never relays
desktop contents: the encrypted payload still goes over the nominated direct
UDP path or the OpenDesk relay fallback.

To control candidate discovery yourself, substitute a self-hosted STUN URL in
the host-core `AgentConfig.urls` configuration. That setting lives in the
client code, not in a relay environment variable.

## Operations and security

Keep the relay up to date, restrict access to the server, and monitor the
connection logs. Don't expect session contents in those logs: the relay sees
routing and network metadata, but it has no session decryption keys.

### Use `wss://` for remote relays

Session payloads are end-to-end encrypted on any transport, so the relay never
sees plaintext. A plain `ws://` relay still exposes connection metadata (IP
addresses, timing, message sizes, the 9-digit IDs being paired) to anyone on
the network path. For a relay reachable over the network, use a `wss://` URL
(TLS). Coolify or your reverse proxy terminates TLS in front of the relay's
single HTTP port, so point clients at `wss://your-relay-host` while the relay
itself listens on plain HTTP behind the proxy. Use plain `ws://` only for
`127.0.0.1`/LAN development or an already-protected network.
