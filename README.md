# Tado° Prometheus Exporter

[![Push on branch](https://github.com/eko/tado-exporter/actions/workflows/master.yml/badge.svg)](https://github.com/eko/tado-exporter/actions/workflows/master.yml)

This is a Prometheus exporter for [tado°](https://www.tado.com/) thermostatic handles.

![Grafana dashboard](https://raw.githubusercontent.com/eko/tado-exporter/master/misc/screenshot.jpg)

## Prerequisites

In case you want to develop on this project, you will need:

* [Rust](https://www.rust-lang.org/)

If you just want to use it, you need nothing apart download and run the binary file in the next step.

## Installation

### Download binary

You can download the latest version of the binary built for your architecture here:

* Architecture **x86_64** [
    [Linux](https://github.com/eko/tado-exporter/releases/latest/download/tado-exporter-x86_64-unknown-linux-gnu.tar.gz)
]
* Architecture **arm** [
    [Linux](https://github.com/eko/tado-exporter/releases/latest/download/tado-exporter-arm-unknown-linux-gnueabihf.tar.gz)
]

### Using Docker

The exporter is also available as a [Docker image](https://hub.docker.com/r/ekofr/tado-exporter).
You can run it using the following example and pass configuration environment variables:

```
$ docker run \
  -v tado-exporter-data:/data \
  -e 'EXPORTER_TICKER=1800' \
  -p '9898:9898' \
  akyrey/tado-exporter:latest
```

Keep the `/data` volume: it stores the tado° refresh token (see [Authentication](#authentication)). Mount a
directory, not the token file itself: the token is replaced atomically by renaming a temporary file.

### From sources

Optionally, you can download and build it from the sources. You have to retrieve the project sources by using one of the following way:
```bash
$ git clone https://github.com/eko/tado-exporter
```

Then, just build the binary:

```
$ cargo build --release
```

## Authentication

tado° no longer accepts username/password login (removed on 2025-03-21). The exporter uses the
OAuth2 **device code flow** instead:

1. On first start (or when the stored token is no longer valid), the exporter logs a line like:
   ```
   WARN tado° authorization required: open https://login.tado.com/oauth2/device?user_code=7BQ5ZQ in a browser and confirm code 7BQ5ZQ (valid for 300s)
   ```
2. Open the URL, log in to tado° and approve the device.
3. The exporter stores the refresh token in `EXPORTER_TOKEN_PATH` (file mode `0600`) and renews
   it automatically. tado° rotates refresh tokens on every use and they expire after 30 days unused,
   so keep this file on persistent storage.

The first poll happens right at startup, so the link appears immediately. If it expires before you approve
it, a new one is logged on the next tick.

## Usage

```bash
$ export EXPORTER_TICKER=1800
$ ./tado-exporter
--- tado° exporter configuration ---
Ticker seconds: 1800
Token path: tado-token.json
------------------------------------
```

### API rate limits

tado° limits REST API usage to **100 requests/day** (20,000/day with an Auto-Assist subscription).
Every tick costs `2 + number of zones` requests (zones list, one state per zone, weather), so choose
`EXPORTER_TICKER` accordingly: e.g. with 4 zones on the free tier, a tick every 1800 seconds uses 288
requests/day, which is already too many — use at least 5400 seconds (90 minutes).

Once the exporter is running, you also have to update your `prometheus.yml` configuration to let it scrape the exporter:

```yaml
scrape_configs:
  - job_name: 'tado'
    static_configs:
      - targets: ['localhost:9898']
```

## Available environment variables

| Environment variable name    | Description                                                                                |
|:----------------------------:|--------------------------------------------------------------------------------------------|
| EXPORTER_TOKEN_PATH    | Optional (default: `tado-token.json`, `/data/tado-token.json` in Docker). Where the refresh token is stored |
| EXPORTER_CLIENT_ID     | Optional (default: `1bb50063-6b0c-4d11-bd99-387f4a91cc46`, tado°'s public web app client). OAuth client id used for the device code flow |
| EXPORTER_TICKER        | Optional (default: 10). This represent the number of seconds the exporter will look for new data. Mind the [rate limits](#api-rate-limits) |
| RUST_LOG               | Optional (default: info). This describes the log level (see https://docs.rs/env_logger/)         |

## Available Prometheus metrics

| Metric name                  | Description                                                                                |
|:----------------------------:|--------------------------------------------------------------------------------------------|
| tado_activity_ac_power_value           | This represent the value (1.0 = ON, 0.0 = OFF) of ac power for every zone        |
| tado_activity_heating_power_percentage | This represent the % of heating power for every zone                             |
| tado_setting_temperature_value         | This represent the current temperature you asked/programmed in a zone            |
| tado_sensor_temperature_value          | This represent the current temperature detected by sensor in a zone              |
| tado_sensor_humidity_percentage        | This represent the current humidity % detected by sensor in a zone               |

## Community welcome

Please feel free to contribute to this project in order to make it evolve. You're very welcome.
