#[macro_use]
extern crate prometheus;

mod automation;
mod config;
mod persist;
mod tado;

use env_logger::{Builder as LoggerBuilder, Env};
use hyper::{service::make_service_fn, service::service_fn, Server};
use log::{error, info};
use std::convert::Infallible;
use std::time::Duration;
use tokio::time::MissedTickBehavior;

use automation::engine::Engine as AutomationEngine;
use automation::executor as automation_executor;
use config::loader as config_loader;
use tado::auth::Authenticator;
use tado::client::Client as TadoClient;
use tado::metrics;
use tado::token_store::TokenStore;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    LoggerBuilder::from_env(Env::default().default_filter_or("info")).init();

    let config = config_loader::load();

    // start ticker
    run_ticker(config);

    // set up http server
    let addr = ([0, 0, 0, 0], 9898).into();
    info!("starting tado° exporter on address: {:?}", addr);

    let make_svc =
        make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(metrics::renderer)) });

    let server = Server::bind(&addr).serve(make_svc);

    // start HTTP server
    if let Err(e) = server.await {
        error!("a server error occurred: {}", e);
    }
}

fn run_ticker(config: config_loader::Config) {
    tokio::spawn(async move {
        let http_client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("HTTP client configuration is valid");
        let authenticator =
            Authenticator::new(http_client.clone(), TokenStore::new(config.token_path))
                .with_client_id(config.client_id);
        let mut tado_client = TadoClient::new(http_client, authenticator);
        let mut automation = AutomationEngine::new(config.automation);

        // Use a ticker instead of sleeping within the loop.
        // This prevents drift as the ticker keeps counting down during refresh, unlike sleep.
        // The first tick fires immediately, so a pending authorization is requested at startup.
        // Ticks missed while waiting for authorization are delayed, not replayed in a burst,
        // to protect the daily API quota.
        let mut ticker = tokio::time::interval(Duration::from_secs(config.ticker));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if tado_client.prepare().await {
                let zones = tado_client.retrieve_zones().await;
                let weather = tado_client.retrieve_weather().await;
                let phones = if automation.needs_phones() {
                    tado_client.retrieve_mobile_devices().await
                } else {
                    None
                };
                metrics::set_zones(&zones);
                metrics::set_weather(weather.as_ref());
                automation_executor::run_tick(
                    &mut automation,
                    &mut tado_client,
                    &zones,
                    weather.as_ref(),
                    phones.as_deref(),
                )
                .await;
            }
            metrics::set_rate_limit(tado_client.rate_limit());
        }
    });
}
