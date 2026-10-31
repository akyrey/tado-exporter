use lazy_static::lazy_static;
use log::{error, info, warn};
use reqwest::StatusCode;
use std::vec::Vec;

use super::auth::Authenticator;
use super::model::{
    MeApiResponse, WeatherApiResponse, ZoneStateApiResponse, ZoneStateResponse, ZonesApiResponse,
};

lazy_static! {
    pub static ref BASE_URL: reqwest::Url = "https://my.tado.com/api/v2/".parse().unwrap();
}

pub struct Client {
    http_client: reqwest::Client,
    base_url: reqwest::Url,
    authenticator: Authenticator,
    access_token: String,
    home_id: i32,
}

impl Client {
    pub fn new(http_client: reqwest::Client, authenticator: Authenticator) -> Client {
        Client::with_base_url(http_client, BASE_URL.clone(), authenticator)
    }

    fn with_base_url(
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
        }
    }

    async fn get(&self, url: reqwest::Url) -> Result<reqwest::Response, reqwest::Error> {
        self.http_client
            .get(url)
            .bearer_auth(&self.access_token)
            .send()
            .await?
            .error_for_status()
    }

    async fn me(&self) -> Result<MeApiResponse, reqwest::Error> {
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

    async fn zone_state(&mut self, zone_id: i32) -> Result<ZoneStateApiResponse, reqwest::Error> {
        let endpoint = format!("/api/v2/homes/{}/zones/{}/state", self.home_id, zone_id);
        let url = self.base_url.join(&endpoint).unwrap();

        let resp = self.get(url).await?;

        resp.json::<ZoneStateApiResponse>().await
    }

    async fn weather(&self) -> Result<WeatherApiResponse, reqwest::Error> {
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

        if e.status() == Some(StatusCode::UNAUTHORIZED) {
            warn!("tado° rejected the access token, it will be renewed on the next tick");
            self.authenticator.invalidate();
        }
    }

    pub async fn retrieve_zones(&mut self) -> Vec<ZoneStateResponse> {
        // retrieve home different zones
        let zones_response = match self.zones().await {
            Ok(resp) => resp,
            Err(e) => {
                self.handle_api_error("unable to retrieve home zones", &e);
                return Vec::new();
            }
        };

        let mut response = Vec::<ZoneStateResponse>::new();

        for zone in zones_response {
            info!("retrieving zone details for {}...", zone.name);
            let zone_state_response = match self.zone_state(zone.id).await {
                Ok(resp) => resp,
                Err(e) => {
                    let context = format!("unable to retrieve home zone '{}' state", zone.name);
                    self.handle_api_error(&context, &e);
                    return Vec::new();
                }
            };

            response.push(ZoneStateResponse {
                name: zone.name,
                state_response: zone_state_response,
            });
        }

        response
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
    use wiremock::matchers::{header, method, path, query_param};
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

        let (client, _dir) = test_client(mock_server.uri().parse().unwrap());

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
        GIVEN an OSM client
        WHEN calling the zone_state() function
        THEN returns the zone state
        */

        // GIVEN
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("api/v2/homes/0/zones/0/state"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(response_str, "application/json"))
            .mount(&mock_server)
            .await;

        let (mut client, _dir) = test_client(mock_server.uri().parse().unwrap());

        // WHEN
        let actual = client.zone_state(0).await.unwrap();

        // THEN
        assert_eq!(actual, expected);
    }
}
