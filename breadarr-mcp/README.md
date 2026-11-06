# breadarr-mcp

MCP stdio server exposing `breadarrd`'s HTTP API to agents. Newline-delimited
JSON-RPC 2.0 on stdin/stdout; logs go to stderr, stdout carries only
protocol frames. Same pattern as `jellyfin-mcp`/`immich-mcp`, but in Rust
so it can reuse `breadarr-shared`'s `DaemonClient` and DTOs directly.

## Run

```sh
cargo run -p breadarr-mcp
```

Reads the same config file as `breadarrd`/`breadarr-tui`
(`~/.config/breadarr/breadarrd.toml`): base URL from
`daemon.listen_addr`, auth from `daemon.api_token`. Env overrides:

- `BREADARR_URL`, e.g. `http://127.0.0.1:7879`
- `BREADARR_API_TOKEN`

## Tools (21)

Library: `library_list`, `library_detail`, `library_health`,
`calendar`, `releases_list`, `stuck_report`, `health_detail`.
Add: `search_catalog` (TVDB/TMDB id lookup), `add_series`, `add_movie`.
Search/grab: `search_now`, `movie_candidates`, `episode_candidates`,
`grab_movie_candidate`, `grab_episode_candidate`.
Review: `review_list`, `review_approve`, `review_reject`.
Monitoring: `set_media_monitored`, `set_episode_monitored`, `remove_media`.

`remove_media` stops tracking (files on disk are kept, same as the
daemon) and requires `confirm: true`. There is deliberately no
file-delete tool in this v1.

## Hestia wiring

- Binary lives at `~/host-mcp/breadarr-mcp` (release build, not
  bakery-managed yet): mounted read-only into Odysseus at
  `/opt/host-mcp/breadarr-mcp` (see `docker-compose.override.yml` in
  `/mnt/media/llm/odysseus/src`) and registered in the `mcp_servers`
  table (transport `stdio`). From inside the container the URL must be
  `http://host.docker.internal:7502`.
- Hestia opencode (`host/hestia.fragment.json` in agent-memory):
  command `{env:HOME}/host-mcp/breadarr-mcp`, `BREADARR_URL`
  `http://127.0.0.1:7502`, token via `{env:BREADARR_API_TOKEN}` from
  `~/.config/opencode/host.env` (mode 600, local-only).
- `breadarrd` binds `0.0.0.0:7502` with a `daemon.api_token` (required
  for any non-loopback bind). ufw has no LAN rule for 7502
  (default-deny holds); only `172.31.0.0/16` (odysseus) is allowed in.
- After `feature/breadarr-mcp` merges and bakery publishes a build
  containing `breadarr-mcp`, switch both configs to the bakery binary
  path and drop `~/host-mcp/breadarr-mcp`.
