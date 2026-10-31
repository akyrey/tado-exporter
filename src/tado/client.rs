use lazy_static::lazy_static;
use log::{error, info, warn};
use reqwest::StatusCode;
use serde_json::json;
use std::collections::HashMap;
use std::vec::Vec;

use super::auth::Authenticator;
use super::model::{
    MeApiResponse, MobileDeviceApiResponse, WeatherApiResponse, ZoneStateApiResponse,
    ZoneStateResponse, ZoneStatesApiResponse, ZonesApiResponse,
};
use super::rate_limit::RateLimit;

lazy_static! {
    pub static ref BASE_URL: reqwest::Url = "https://my.tado.com/api/v2/".parse().unwrap();
}

pub struct Client {
    http_client: reqwest::Client,
    base_url: reqwest::Url,
    authenticator: Authenticator,
    access_token: String,
    home_id: i32,
    /// Zone names by id. Names rarely change, so they are only re-fetched when an
    /// unknown zone id shows up, which keeps every tick at a single zone request.
    zone_names: HashMap<i32, String>,
    rate_limit: Option<RateLimit>,
}

impl Client {
    pub fn new(http_client: reqwest::Client, authenticator: Authenticator) -> Client {
        Client::with_base_url(http_client, BASE_URL.clone(), authenticator)
    }

    pub(crate) fn with_base_url(
        http_client: reqwest::Client,
        base_url: reqwest::Url,
        authenticator: Authenticator,
    ) -> Client {
        Client {
            http_client,
            base_url,
            authenticator,
            access_token: String::default(),
            home_id: 0,
            zone_names: HashMap::new(),
            rate_limit: None,
        }
    }

    /// The API quota reported by the most recent response that carried quota headers.
    pub fn rate_limit(&self) -> Option<&RateLimit> {
        self.rate_limit.as_ref()
    }

    async fn get(&mut self, url: reqwest::Url) -> Result<reqwest::Response, reqwest::Error> {
        let request = self.http_client.get(url);
        self.send(request).await
    }

    /// Sends an authenticated request, recording the quota headers of the response.
    async fn send(
        &mut self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let response = request.bearer_auth(&self.access_token).send().await?;

        // Record the quota before checking the status: a 429 still reports when it refills.
        if let Some(rate_limit) = RateLimit::from_headers(response.headers()) {
            self.rate_limit = Some(rate_limit);
        }

        response.error_for_status()
    }

    async fn me(&mut self) -> Result<MeApiResponse, reqwest::Error> {
        let url = self.base_url.join("/api/v2/me").unwrap();
        let resp = self.get(url).await?;

        resp.json::<MeApiResponse>().await
    }

    async fn zones(&mut self) -> Result<Vec<ZonesApiResponse>, reqwest::Error> {
        let endpoint = format!("/api/v2/homes/{}/zones", self.home_id);
        let url = self.base_url.join(&endpoint).unwrap();

        let resp = self.get(url).await?;

        resp.json::<Vec<ZonesApiResponse>>().await
    }

    /// Fetches the state of every zone in a single request.
    async fn zone_states(&mut self) -> Result<Vec<(i32, ZoneStateApiResponse)>, reqwest::Error> {
        let endpoint = format!("/api/v2/homes/{}/zoneStates", self.home_id);
        let url = self.base_url.join(&endpoint).unwrap();

        let resp = self.get(url).await?;
        let body = resp.json::<ZoneStatesApiResponse>().await?;

        let states = body
            .zoneStates
            .into_iter()
            .filter_map(|(id, state)| match id.parse::<i32>() {
                Ok(id) => Some((id, state)),
                Err(_) => {
                    warn!("ignoring zone state with non-numeric id {:?}", id);
                    None
                }
            })
            .collect();

        Ok(states)
    }

    async fn weather(&mut self) -> Result<WeatherApiResponse, reqwest::Error> {
        let endpoint = format!("homes/{}/weather/", self.home_id);
        let url = self.base_url.join(&endpoint).unwrap();

        let resp = self.get(url).await?;

        resp.json::<WeatherApiResponse>().await
    }

    /// Ensures a valid access token and a known home identifier. Call it once per tick before
    /// the `retrieve_*` functions; returns `false` (after logging why) when the API cannot be
    /// called yet. A missing authorization blocks here while the device flow is pending.
    pub async fn prepare(&mut self) -> bool {
        self.access_token = match self.authenticator.access_token().await {
            Ok(token) => token,
            Err(e) => {
                error!("unable to authenticate: {}", e);
                return false;
            }
        };

        if self.home_id != 0 {
            return true;
        }

        match self.me().await {
            Ok(me) => match me.homes.first() {
                Some(home) => {
                    self.home_id = home.id;
                    true
                }
                None => {
                    error!("the tado° account has no home");
                    false
                }
            },
            Err(e) => {
                self.handle_api_error("unable to retrieve home identifier", &e);
                false
            }
        }
    }

    fn handle_api_error(&mut self, context: &str, e: &reqwest::Error) {
        error!("{}: {}", context, e);

        match e.status() {
            Some(StatusCode::UNAUTHORIZED) => {
                warn!("tado° rejected the access token, it will be renewed on the next tick");
                self.authenticator.invalidate();
            }
            Some(StatusCode::TOO_MANY_REQUESTS) => {
                let refill = self
                    .rate_limit
                    .and_then(|limit| limit.reset_seconds)
                    .map_or_else(|| "unknown".to_string(), |s| format!("{}s", s));
                warn!(
                    "tado° API quota exhausted (refills in {}), consider a longer EXPORTER_TICKER",
                    refill
                );
            }
            _ => {}
        }
    }

    async fn refresh_zone_names(&mut self) -> Result<(), reqwest::Error> {
        info!("retrieving zone names...");
        let zones = self.zones().await?;
        self.zone_names = zones.into_iter().map(|zone| (zone.id, zone.name)).collect();
        Ok(())
    }

    pub async fn retrieve_zones(&mut self) -> Vec<ZoneStateResponse> {
        info!("retrieving zone states...");
        let states = match self.zone_states().await {
            Ok(states) => states,
            Err(e) => {
                self.handle_api_error("unable to retrieve zone states", &e);
                return Vec::new();
            }
        };

        let has_unknown_zone = states
            .iter()
            .any(|(id, _)| !self.zone_names.contains_key(id));
        if has_unknown_zone {
            if let Err(e) = self.refresh_zone_names().await {
                self.handle_api_error("unable to retrieve home zones", &e);
                return Vec::new();
            }
        }

        let mut zones: Vec<ZoneStateResponse> = states
            .into_iter()
            .filter_map(|(id, state)| match self.zone_names.get(&id) {
                Some(name) => Some(ZoneStateResponse {
                    id,
                    name: name.clone(),
                    state_response: state,
                }),
                None => {
                    warn!("ignoring state of zone {} missing from the zone list", id);
                    None
                }
            })
            .collect();
        zones.sort_by(|a, b| a.name.cmp(&b.name));

        zones
    }

    /// Phones registered for geofencing, with their location if they share it.
    pub async fn retrieve_mobile_devices(&mut self) -> Option<Vec<MobileDeviceApiResponse>> {
        let endpoint = format!("/api/v2/homes/{}/mobileDevices", self.home_id);
        let url = self.base_url.join(&endpoint).unwrap();

        let result = match self.get(url).await {
            Ok(resp) => resp.json::<Vec<MobileDeviceApiResponse>>().await,
            Err(e) => Err(e),
        };

        match result {
            Ok(devices) => Some(devices),
            Err(e) => {
                self.handle_api_error("unable to retrieve mobile devices", &e);
                None
            }
        }
    }

    fn zone_url(&self, zone_id: i32, suffix: &str) -> reqwest::Url {
        let endpoint = format!(
            "/api/v2/homes/{}/zones/{}/{}",
            self.home_id, zone_id, suffix
        );
        self.base_url.join(&endpoint).unwrap()
    }

    /// Replaces the zone's schedule with heating off until changed again.
    pub async fn set_heating_off(&mut self, zone_id: i32) -> Result<(), reqwest::Error> {
        let body = json!({
            "setting": { "type": "HEATING", "power": "OFF" },
            "termination": { "typeSkillBasedApp": "MANUAL" }
        });
        let request = self
            .http_client
            .put(self.zone_url(zone_id, "overlay"))
            .json(&body);
        self.send(request).await.map(|_| ())
    }

    /// Heats the zone to `celsius` for `minutes`, after which tado° resumes the schedule.
    pub async fn set_temperature_for(
        &mut self,
        zone_id: i32,
        celsius: f64,
        minutes: u64,
    ) -> Result<(), reqwest::Error> {
        let body = json!({
            "setting": { "type": "HEATING", "power": "ON", "temperature": { "celsius": celsius } },
            "termination": { "typeSkillBasedApp": "TIMER", "durationInSeconds": minutes * 60 }
        });
        let request = self
            .http_client
            .put(self.zone_url(zone_id, "overlay"))
            .json(&body);
        self.send(request).await.map(|_| ())
    }

    /// Removes the zone's overlay so it follows its schedule again.
    pub async fn resume_schedule(&mut self, zone_id: i32) -> Result<(), reqwest::Error> {
        let request = self.http_client.delete(self.zone_url(zone_id, "overlay"));
        self.send(request).await.map(|_| ())
    }

    /// Confirms a detected open window: heating pauses for the zone's configured timeout.
    pub async fn activate_open_window(&mut self, zone_id: i32) -> Result<(), reqwest::Error> {
        let url = self.zone_url(zone_id, "state/openWindow/activate");
        let request = self.http_client.post(url);
        self.send(request).await.map(|_| ())
    }

    /// Sets the home's presence (`HOME` or `AWAY`), overriding geofencing.
    pub async fn set_presence(&mut self, presence: &str) -> Result<(), reqwest::Error> {
        let endpoint = format!("/api/v2/homes/{}/presenceLock", self.home_id);
        let url = self.base_url.join(&endpoint).unwrap();
        let request = self
            .http_client
            .put(url)
            .json(&json!({ "homePresence": presence }));
        self.send(request).await.map(|_| ())
    }

    /// Logs a failed write and reacts to authentication/quota errors.
    pub fn report_write_error(&mut self, context: &str, e: &reqwest::Error) {
        self.handle_api_error(context, e);
    }

    pub async fn retrieve_weather(&mut self) -> Option<WeatherApiResponse> {
        info!("retrieving weather details ...");

        match self.weather().await {
            Ok(resp) => Some(resp),
            Err(e) => {
                self.handle_api_error("unable to retrieve weather info", &e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tado::model::{
        ActivityDataPointsHeatingPowerApiResponse, SensorDataPointsHumidityApiResponse,
        SensorDataPointsInsideTemperatureApiResponse, WeatherOutsideTemperatureApiResponse,
        WeatherSolarIntensityApiResponse, ZoneStateActivityDataPointsApiResponse,
        ZoneStateApiResponse, ZoneStateOpenWindowApiResponse, ZoneStateSensorDataPointsApiResponse,
        ZoneStateSettingApiResponse, ZoneStateSettingTemperatureApiResponse,
    };

    use crate::tado::token_store::TokenStore;
    use rstest::*;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client(base_url: reqwest::Url) -> (Client, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path().join("token.json"));
        let http_client = reqwest::Client::new();
        let authenticator = Authenticator::new(http_client.clone(), store);
        (
            Client::with_base_url(http_client, base_url, authenticator),
            dir,
        )
    }

    #[test]
    fn test_new() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path().join("token.json"));
        let client = Client::new(
            reqwest::Client::new(),
            Authenticator::new(reqwest::Client::new(), store),
        );

        assert_eq!(client.base_url, *BASE_URL);
        assert_eq!(client.home_id, 0);
    }

    #[test]
    fn test_with_base_url() {
        let (client, _dir) = test_client("https://example.com".parse().unwrap());

        assert_eq!(client.base_url, "https://example.com".parse().unwrap());
    }

    async fn authenticated_client(server: &MockServer, dir: &TempDir) -> Client {
        let store = TokenStore::new(dir.path().join("token.json"));
        store.save("refresh-1").unwrap();
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(query_param("refresh_token", "refresh-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-1",
                "expires_in": 599,
                "refresh_token": "refresh-2"
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .and(query_param("refresh_token", "refresh-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-2",
                "expires_in": 599,
                "refresh_token": "refresh-3"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/me"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "homes": [{ "id": 42 }] })),
            )
            .expect(1)
            .mount(server)
            .await;

        let http_client = reqwest::Client::new();
        let auth_url = format!("{}/oauth2/", server.uri()).parse().unwrap();
        let authenticator = Authenticator::with_base_url(http_client.clone(), auth_url, store);
        Client::with_base_url(http_client, server.uri().parse().unwrap(), authenticator)
    }

    #[tokio::test]
    async fn retrieve_weather_authenticates_and_resolves_home_once() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path("/homes/42/weather/"))
            .and(header("Authorization", "Bearer access-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "solarIntensity": { "percentage": 10.0 },
                "outsideTemperature": { "celsius": 5.0, "fahrenheit": 41.0 }
            })))
            .expect(2)
            .mount(&server)
            .await;
        let mut client = authenticated_client(&server, &dir).await;

        // Act
        assert!(client.prepare().await);
        let first = client.retrieve_weather().await;
        assert!(client.prepare().await);
        let second = client.retrieve_weather().await;

        // Assert
        assert!(first.is_some());
        assert!(second.is_some());
        assert_eq!(client.home_id, 42);
    }

    fn zone_state_json(celsius: f64) -> serde_json::Value {
        json!({
            "setting": { "type": "HEATING", "temperature": null },
            "activityDataPoints": { "heatingPower": { "percentage": 0.0 } },
            "sensorDataPoints": {
                "insideTemperature": { "celsius": celsius, "fahrenheit": 0.0 },
                "humidity": { "percentage": 50.0 }
            }
        })
    }

    #[tokio::test]
    async fn retrieve_zones_uses_one_request_per_tick_and_caches_names() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path("/api/v2/homes/42/zoneStates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "zoneStates": { "1": zone_state_json(20.5), "2": zone_state_json(18.0) }
            })))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/homes/42/zones"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "name": "Salotto" },
                { "id": 2, "name": "Bagno" }
            ])))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = authenticated_client(&server, &dir).await;
        assert!(client.prepare().await);

        // Act
        let first = client.retrieve_zones().await;
        let second = client.retrieve_zones().await;

        // Assert
        let names: Vec<&str> = first.iter().map(|zone| zone.name.as_str()).collect();
        assert_eq!(names, vec!["Bagno", "Salotto"]);
        assert_eq!(second.len(), 2);
    }

    #[tokio::test]
    async fn records_rate_limit_from_responses_including_errors() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path("/homes/42/weather/"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("ratelimit", "\"perday\";r=0;t=600")
                    .insert_header("ratelimit-policy", "\"perday\";q=100;w=86400"),
            )
            .mount(&server)
            .await;
        let mut client = authenticated_client(&server, &dir).await;
        assert!(client.prepare().await);

        // Act
        let weather = client.retrieve_weather().await;

        // Assert
        assert!(weather.is_none());
        assert_eq!(
            client.rate_limit(),
            Some(&RateLimit {
                remaining: Some(0),
                reset_seconds: Some(600),
                quota: Some(100),
            })
        );
    }

    async fn prepared_client(server: &MockServer, dir: &TempDir) -> Client {
        let mut client = authenticated_client(server, dir).await;
        assert!(client.prepare().await);
        client
    }

    #[tokio::test]
    async fn set_heating_off_puts_manual_off_overlay() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("PUT"))
            .and(path("/api/v2/homes/42/zones/3/overlay"))
            .and(header("Authorization", "Bearer access-1"))
            .and(body_json(json!({
                "setting": { "type": "HEATING", "power": "OFF" },
                "termination": { "typeSkillBasedApp": "MANUAL" }
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let result = client.set_heating_off(3).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn set_temperature_for_puts_timer_overlay() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("PUT"))
            .and(path("/api/v2/homes/42/zones/4/overlay"))
            .and(body_json(json!({
                "setting": { "type": "HEATING", "power": "ON", "temperature": { "celsius": 21.0 } },
                "termination": { "typeSkillBasedApp": "TIMER", "durationInSeconds": 3600 }
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let result = client.set_temperature_for(4, 21.0, 60).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn resume_schedule_deletes_overlay() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("DELETE"))
            .and(path("/api/v2/homes/42/zones/2/overlay"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let result = client.resume_schedule(2).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn activate_open_window_posts_activation() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path("/api/v2/homes/42/zones/1/state/openWindow/activate"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let result = client.activate_open_window(1).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn set_presence_puts_presence_lock() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("PUT"))
            .and(path("/api/v2/homes/42/presenceLock"))
            .and(body_json(json!({ "homePresence": "AWAY" })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let result = client.set_presence("AWAY").await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn failed_write_returns_error() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("PUT"))
            .and(path("/api/v2/homes/42/zones/3/overlay"))
            .respond_with(ResponseTemplate::new(422))
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let result = client.set_heating_off(3).await;

        // Assert
        assert_eq!(
            result.unwrap_err().status(),
            Some(StatusCode::UNPROCESSABLE_ENTITY)
        );
    }

    #[tokio::test]
    async fn retrieve_mobile_devices_parses_locations() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path("/api/v2/homes/42/mobileDevices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "name": "a", "settings": { "geoTrackingEnabled": true },
                  "location": { "atHome": false, "stale": false } },
                { "id": 2, "name": "b", "settings": { "geoTrackingEnabled": false } }
            ])))
            .mount(&server)
            .await;
        let mut client = prepared_client(&server, &dir).await;

        // Act
        let devices = client.retrieve_mobile_devices().await.unwrap();

        // Assert
        assert_eq!(devices.len(), 2);
        assert!(devices[0].settings.geoTrackingEnabled);
        assert_eq!(devices[0].location.as_ref().map(|l| l.atHome), Some(false));
        assert!(devices[1].location.is_none());
    }

    #[tokio::test]
    async fn unauthorized_response_forces_token_renewal() {
        // Arrange
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path("/homes/42/weather/"))
            .and(header("Authorization", "Bearer access-1"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/homes/42/weather/"))
            .and(header("Authorization", "Bearer access-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "solarIntensity": { "percentage": 10.0 },
                "outsideTemperature": { "celsius": 5.0, "fahrenheit": 41.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = authenticated_client(&server, &dir).await;

        // Act
        assert!(client.prepare().await);
        let rejected = client.retrieve_weather().await;
        assert!(client.prepare().await);
        let renewed = client.retrieve_weather().await;

        // Assert
        assert!(rejected.is_none());
        assert!(renewed.is_some());
    }

    #[rstest(response_str, expected,
        case(
            r#"
            {
                "solarIntensity": {
                  "type": "PERCENTAGE",
                  "percentage": 18.3,
                  "timestamp": "2022-09-03T17:43:41.088Z"
                },
                "outsideTemperature": {
                  "celsius": 21.53,
                  "fahrenheit": 70.75,
                  "timestamp": "2022-09-03T17:43:41.088Z",
                  "type": "TEMPERATURE",
                  "precision": { "celsius": 0.01, "fahrenheit": 0.01 }
                },
                "weatherState": {
                  "type": "WEATHER_STATE",
                  "value": "CLOUDY_PARTLY",
                  "timestamp": "2022-09-03T17:43:41.088Z"
                }
              }
            "#,
            WeatherApiResponse {
                solarIntensity: WeatherSolarIntensityApiResponse {
                    percentage: 18.3,
                },
                outsideTemperature: WeatherOutsideTemperatureApiResponse{
                    celsius: 21.53,
                    fahrenheit: 70.75
                },
            }
        )
    )]
    #[actix_rt::test]
    async fn test_weather(response_str: &str, expected: WeatherApiResponse) {
        /*
        GIVEN an OSM client
        WHEN calling the capabilities() function
        THEN returns the sets of capablities and policies
        */

        // GIVEN
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("homes/0/weather/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(response_str, "application/json"))
            .mount(&mock_server)
            .await;

        let (mut client, _dir) = test_client(mock_server.uri().parse().unwrap());

        // WHEN
        let actual = client.weather().await.unwrap();

        // THEN
        assert_eq!(actual, expected);
    }

    #[rstest(response_str, expected,
        case(
            r#"{
                "setting":{
                  "type":"tado",
                  "temperature":{
                    "celsius":21.53,
                    "fahrenheit":70.75
                  }
                },
                "activityDataPoints":{
                  "heatingPower":{
                    "percentage":0.0
                  },
                  "acPower":null
                },
                "sensorDataPoints":{
                  "insideTemperature":{
                    "celsius":25.0,
                    "fahrenheit":77.0
                  },
                  "humidity":{
                    "percentage":75.0
                  }
                }
              }"#,
            ZoneStateApiResponse {
                setting : ZoneStateSettingApiResponse {
                    deviceType: "tado".to_string(),
                    power: None,
                    temperature: Some(ZoneStateSettingTemperatureApiResponse {
                        celsius: 21.53,
                        fahrenheit: 70.75
                    })
                },
                activityDataPoints : ZoneStateActivityDataPointsApiResponse {
                    heatingPower : Some(ActivityDataPointsHeatingPowerApiResponse {
                        percentage: 0.0
                    }),
                    acPower : None
                },
                openWindow: None,
                tadoMode: None,
                overlay: None,
                openWindowDetected: None,
                sensorDataPoints: ZoneStateSensorDataPointsApiResponse {
                    insideTemperature : Some(SensorDataPointsInsideTemperatureApiResponse {
                        celsius: 25.0,
                        fahrenheit: 77.0
                    }),
                    humidity : Some(SensorDataPointsHumidityApiResponse {
                        percentage: 75.0
                    })
                }
            }
        ),
        case(
            r#"{
                "setting":{
                  "type":"tado",
                  "temperature":{
                    "celsius":21.53,
                    "fahrenheit":70.75
                  }
                },
                "openWindow":{
                    "detectedTime":"2022-11-21T11:15:32Z",
                    "durationInSeconds":900,
                    "expiry":"2022-11-21T11:30:32Z",
                    "remainingTimeInSeconds":662
                },
                "activityDataPoints":{
                  "heatingPower":{
                    "percentage":0.0
                  },
                  "acPower":null
                },
                "sensorDataPoints":{
                  "insideTemperature":{
                    "celsius":25.0,
                    "fahrenheit":77.0
                  },
                  "humidity":{
                    "percentage":75.0
                  }
                }
              }"#,
            ZoneStateApiResponse {
                setting : ZoneStateSettingApiResponse {
                    deviceType: "tado".to_string(),
                    power: None,
                    temperature: Some(ZoneStateSettingTemperatureApiResponse {
                        celsius: 21.53,
                        fahrenheit: 70.75
                    })
                },
                openWindow : Some(ZoneStateOpenWindowApiResponse {
                    detectedTime: "2022-11-21T11:15:32Z".to_string(),
                    durationInSeconds: 900,
                    expiry: "2022-11-21T11:30:32Z".to_string(),
                    remainingTimeInSeconds: 662
                }),
                activityDataPoints : ZoneStateActivityDataPointsApiResponse {
                    heatingPower : Some(ActivityDataPointsHeatingPowerApiResponse {
                        percentage: 0.0
                    }),
                    acPower : None
                },
                tadoMode: None,
                overlay: None,
                openWindowDetected: None,
                sensorDataPoints: ZoneStateSensorDataPointsApiResponse {
                    insideTemperature : Some(SensorDataPointsInsideTemperatureApiResponse {
                        celsius: 25.0,
                        fahrenheit: 77.0
                    }),
                    humidity : Some(SensorDataPointsHumidityApiResponse {
                        percentage: 75.0
                    })
                }
            }
        )
    )]
    #[actix_rt::test]
    async fn test_zone_state(response_str: &str, expected: ZoneStateApiResponse) {
        /*
        GIVEN a zoneStates response wrapping a zone state
        WHEN calling the zone_states() function
        THEN returns the parsed zone state with its numeric id
        */

        // GIVEN
        let mock_server = MockServer::start().await;
        let body = format!(r#"{{"zoneStates": {{"1": {}}}}}"#, response_str);

        Mock::given(method("GET"))
            .and(path("api/v2/homes/0/zoneStates"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .mount(&mock_server)
            .await;

        let (mut client, _dir) = test_client(mock_server.uri().parse().unwrap());

        // WHEN
        let actual = client.zone_states().await.unwrap();

        // THEN
        assert_eq!(actual, vec![(1, expected)]);
    }
}
