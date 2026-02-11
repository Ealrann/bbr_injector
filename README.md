# bbr-injector

Injects compact-proof gossip messages to a fixed set of Chia full nodes, using proofs from a local archive DB created by `bbr_scrap`.

It can:
- send `new_compact_vdf` announcements at a controlled rate, and
- answer inbound `request_compact_vdf` with `respond_compact_vdf` from the local archive.

The process includes a small local web UI and HTTP control API.

## What This Is (and Is Not)

This tool is a lightweight peer/injector. It is **not** a full node.

- Input source: local SQLite archive (`compact_proofs`) from `bbr_scrap`.
- Network behavior: connects only to configured peers in `peers.targets`.
- State persistence: writes local runtime state (`cursor`, speed, mode, paused) to `data/in_flight.json`.

## Requirements

- Rust toolchain (`cargo`)
- `pnpm` (for the UI build)
- Access to a Chia TLS cert/key pair signed by the Chia CA
- A local proof archive DB from `bbr_scrap`:
  - default expected path in example config: `../bbr_scrap/data/compact_proofs.sqlite`

## Configuration

Start from:

```bash
cp config.example.toml config.toml
```

Important fields in `config.toml`:

- `network_id`: must match target peers (`mainnet`, etc.)
- `[tls].cert_path` / `[tls].key_path`: client cert/key used for WSS peer connections
- `[archive].db_path`: path to `bbr_scrap` SQLite archive
- `[peers].targets`: fixed list of `host:port` full-node peers to connect to
- `[http].listen_addr`: local UI/API bind address (default `127.0.0.1:6182`)
- `[storage].in_flight_path`: persisted runtime state JSON

Optional:
- `protocol_version` (normally leave unset; defaults to `0.0.36`)

## Run

### Fast path (build UI + run)

```bash
./dev.sh config.toml
```

This script:
- builds UI (`ui/dist`)
- runs injector with `cargo run -- --config ...`

### Manual path

```bash
cd ui
pnpm install
pnpm build
cd ..
RUST_LOG=info cargo run -- --config config.toml
```

### tmux / long-running

```bash
./run_tmux.sh config.toml
tmux attach -t bbr_injector
```

## UI and Control

Open:

- `http://127.0.0.1:6182` (or your configured `http.listen_addr`)

The injector starts in **paused** mode on every process start. Use the UI button to start.

Main controls:

- `cursor_event_id` and `end_event_id` (optional upper bound)
- speed: `proofs_per_window` and `window_ms`
- send mode:
  - `announcement` (normal): sends `new_compact_vdf`
  - `respond_only` (injection mode): sends `respond_compact_vdf` payloads
    - this is specialized behavior for controlled experiments, not standard gossip flow

## HTTP API (same controls as UI)

Base: `http://127.0.0.1:6182/api`

- `GET /status`
- `POST /control/pause` with `{ "paused": true|false }`
- `POST /control/speed` with `{ "proofs_per_window": N, "window_ms": N }`
- `POST /control/cursor` with `{ "cursor_event_id": N, "end_event_id": N|null }`
- `POST /control/mode` with `{ "send_mode": "announcement" | "respond_only" }`

Examples:

```bash
curl -s http://127.0.0.1:6182/api/status | jq

curl -X POST http://127.0.0.1:6182/api/control/cursor \
  -H 'content-type: application/json' \
  -d '{"cursor_event_id": 1000000, "end_event_id": 1005000}'

curl -X POST http://127.0.0.1:6182/api/control/speed \
  -H 'content-type: application/json' \
  -d '{"proofs_per_window": 3, "window_ms": 2000}'

curl -X POST http://127.0.0.1:6182/api/control/mode \
  -H 'content-type: application/json' \
  -d '{"send_mode":"announcement"}'

curl -X POST http://127.0.0.1:6182/api/control/pause \
  -H 'content-type: application/json' \
  -d '{"paused":false}'
```

## Typical One-Shot Workflow

1. Make sure `bbr_scrap` archive DB is available and current.
2. Configure peer targets and TLS paths.
3. Start injector (`./dev.sh` or `./run_tmux.sh`).
4. In UI/API, set:
   - mode (`announcement` for normal gossip propagation),
   - cursor/end range,
   - speed.
5. Unpause and monitor:
   - connected peers,
   - cursor progress,
   - proof-serving metrics.
6. Pause when done.

## Runtime Behavior Notes

- Archive DB is read-only from injector perspective.
- Peer target hostnames are resolved at startup; at least one must resolve.
- Throughput is rate-limited by `proofs_per_window / window_ms`.
- If outbound channels are full, events are retried later.
- Cursor is persisted continuously in `data/in_flight.json`.
- On startup, injector forces `paused=true` for safety.

## Troubleshooting

- UI says missing index:
  - build UI (`cd ui && pnpm build`)
- TLS/connect failures:
  - verify `cert_path` and `key_path`
  - verify peer is reachable and running full-node WSS
  - verify `network_id` matches peer network
- No progress:
  - confirm connected peers > 0 in `/api/status`
  - confirm cursor/end range has available events in archive
  - confirm mode/speed and paused state

## Optional test mode (toy nodes)

There is an optional integration flow behind `--features test-mode`.

Example:

```bash
cargo run --features test-mode -- --config config.toy.toml test \
  --toy-a http://127.0.0.1:8080 \
  --toy-b http://127.0.0.1:8081 \
  --count 2
```
