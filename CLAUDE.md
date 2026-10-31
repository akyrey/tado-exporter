# CLAUDE.md

Prometheus exporter for tado° smart thermostats, written in Rust. Fork of `eko/tado-exporter` (origin: `akyrey/tado-exporter`, default branch `master`). Docker image is published as `akyrey/tado-exporter`.

## Commands

```bash
cargo build                       # debug build
cargo test                        # unit tests (wiremock-based HTTP mocks, no network)
cargo clippy -- -D warnings       # CI fails on any warning
cargo fmt --all -- --check        # CI enforces rustfmt
docker buildx build --platform linux/amd64,linux/arm64,linux/arm/v7 .   # multi-arch image
```

Rust is installed via rustup (`~/.cargo/bin`); if `cargo` isn't on PATH, prepend it.

## Layout

| Path | Role |
|---|---|
| `src/main.rs` | Starts a tokio interval ticker (polls tado) + hyper 0.14 HTTP server on `0.0.0.0:9898` serving metrics on every path |
| `src/config/loader.rs` | Env-var config (`EXPORTER_TICKER`, `EXPORTER_TOKEN_PATH`, `EXPORTER_CLIENT_ID`). Warns if the removed `EXPORTER_USERNAME`/`PASSWORD`/`CLIENT_SECRET` are set |
| `src/tado/auth.rs` | `Authenticator`: OAuth2 device code flow + refresh against `login.tado.com`, caches the access token in memory |
| `src/tado/token_store.rs` | Persists the rotating refresh token as JSON (atomic write, mode `0600`) |
| `src/tado/client.rs` | tado REST client: `/me`, `/homes/{id}/zoneStates` (all zones in one call), `/homes/{id}/zones` (names, cached, re-fetched only for unknown ids), `/weather`. `prepare()` obtains a token and the home id once per tick. Every response's quota headers are recorded |
| `src/tado/rate_limit.rs` | Parses `ratelimit` / `ratelimit-policy` headers into `RateLimit` |
| `src/tado/model.rs` | serde response structs (camelCase fields with `#[allow(non_snake_case)]`) |
| `src/tado/metrics.rs` | `lazy_static` Prometheus `GaugeVec`s, `set_zones`/`set_weather`, `/metrics` renderer |
| `src/automation/` | Thermostat automation. `mod.rs`: `Snapshot`/`ZoneControl`/`Action`. `engine.rs`: builds snapshots, runs enabled rules in priority order (first action per zone wins). `executor.rs`: applies decisions or logs them in dry-run. `state.rs`: persisted `AutomationState` and owned-overlay tracking. `rules/`: `open_window`, `humidity`, `season`, `presence` |
| `src/persist.rs` | Atomic 0600 file writes shared by the token store and automation state |

## Authentication and API constraints (read before changing the client)

- **Device code grant only.** tado° removed the password grant on 2025-03-21. `auth.rs` implements:
  - `POST https://login.tado.com/oauth2/device_authorize` (`client_id=1bb50063-6b0c-4d11-bd99-387f4a91cc46`, `scope=offline_access`). The exporter then logs the browser URL the user must approve.
  - Polling `POST .../oauth2/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code` (handles `authorization_pending`, `slow_down`, `access_denied`, `expired_token`).
  - Renewal with `grant_type=refresh_token`.
  - OAuth params are sent as **query parameters**, as in tado°'s docs.
- **Refresh tokens rotate**: each use revokes the old one, and an unused token expires after 30 days. Always persist the new token (`TokenStore::save`) and keep it in memory too. Only an OAuth rejection (4xx) discards the stored token. Network errors and 5xx keep it.
- A 401 from the REST API calls `Authenticator::invalidate()`, which forces a refresh on the next tick.
- **Rate limits** (tado°, since early 2026): officially 100 req/day on the free tier and 20,000/day with Auto-Assist. The maintainer's free account actually reported **1,000/day** on 2026-10-31. The quota comes from the `ratelimit` (`"perday";r=<left>[;t=<secs-to-refill>]`) and `ratelimit-policy` (`"perday";q=<quota>;w=<window>`) headers, exposed as `tado_api_ratelimit_*` metrics. Each tick costs **2** REST calls (zoneStates + weather), plus `/me` and `/zones` at startup. Token calls go to `login.tado.com`. Budget every new API call.
- The maintainer has **V3+** devices (`my.tado.com/api/v2`). tado° X devices (`hops.tado.com/.../rooms`) are not supported.

## Automation invariants

- **Manual always wins.** A zone's overlay counts as *owned* only if `AutomationState::owned_overlays` records it **and** it still matches (`state::zone_control`). Rules may change `Schedule`, `Owned` and `BoostEnded` zones, never `Manual` ones. An owned boost that disappears before its `until` (minus slack) was cancelled by hand, so it becomes `Schedule`, not `BoostEnded`.
- **Ownership changes only after a successful live write** (`record_applied`). `reconcile` runs in the executor after applying. It drops vanished or replaced overlays, but keeps zones missing from the snapshot and ended boosts whose follow-up action was not applied (`pending_restore`).
- **Unapplied decisions are retried.** A failed write, or an action dropped by `without_conflicts`, restores that rule's previous state (`with_rules_from`), so the rule decides again next tick.
- **Rules are pure:** `evaluate(config, snapshot, state) -> (Vec<Action>, NextState)`, with no I/O or clock reads (time comes from `Snapshot::now`). Test them with hand-built snapshots.
- **Act on edges, not levels.** Season and presence act on *changes*, so a manual change in the app persists. Level-triggered retries may only touch owned overlays. Presence adopts its first observation without acting.
- **Dry-run (default) never writes to the API,** but rule state still advances. The state records its mode, and `for_mode` resets edge memory when the mode changes. `executor` tests cover live success, failure, dry-run and the dry-run to live switch against wiremock.
- **Rule priority:** open_window, humidity, season, presence. When the season rule is enabled, it alone decides what follows a boost (humidity's `restore_after_boost` is off).
- **Every extra read counts against the daily quota.** `Engine::needs_phones` gates the mobile-devices request.

## Conventions

- Rust edition 2018, async with tokio. Errors in the client are logged and turned into an empty/`None` result. They are not propagated.
- Tests follow Arrange/Act/Assert, use `rstest` cases + `wiremock::MockServer`, and `tempfile::TempDir` for the token file. Build clients with `Client::with_base_url` / `Authenticator::with_base_url` pointing at the mock server (auth base must end in `/oauth2/`).
- The Prometheus registry is global and shared by every test in the binary. Metric tests look families up by name (`gauge_values`) and never assert on the whole registry. Tests that snapshot a gauge take `WEATHER_LOCK`.
- Commit messages follow `<type>(<scope>): <description>`.
