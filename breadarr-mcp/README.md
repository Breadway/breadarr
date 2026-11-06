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
