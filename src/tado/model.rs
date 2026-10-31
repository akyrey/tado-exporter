use serde_derive::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Debug)]
pub struct MeApiResponse {
    pub homes: Vec<HomesApiResponse>,
}

#[derive(Deserialize, Debug)]
pub struct HomesApiResponse {
    pub id: i32,
}

#[derive(Deserialize, Debug)]
pub struct ZonesApiResponse {
    pub id: i32,
    pub name: String,
}

#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct ZoneStateApiResponse {
    pub setting: ZoneStateSettingApiResponse,
    pub activityDataPoints: ZoneStateActivityDataPointsApiResponse,
    pub sensorDataPoints: ZoneStateSensorDataPointsApiResponse,
    pub openWindow: Option<ZoneStateOpenWindowApiResponse>,
    /// `HOME` or `AWAY`: the home's presence mode as applied to this zone.
    #[serde(default)]
    pub tadoMode: Option<String>,
    /// A manual or timed setting replacing the schedule, if any.
    #[serde(default)]
    pub overlay: Option<ZoneOverlayApiResponse>,
    /// tado° detected an open window that has not been activated yet (no Auto-Assist).
    #[serde(default)]
    pub openWindowDetected: Option<bool>,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct ZoneOverlayApiResponse {
    pub setting: ZoneStateSettingApiResponse,
    pub termination: OverlayTerminationApiResponse,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
pub struct OverlayTerminationApiResponse {
    /// `MANUAL`, `TIMER` or `TADO_MODE` (until the next schedule block).
    #[serde(rename = "type")]
    pub termination_type: String,
}

/// Response of `/homes/{id}/zoneStates`: the state of every zone, keyed by zone id.
#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct ZoneStatesApiResponse {
    pub zoneStates: HashMap<String, ZoneStateApiResponse>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[allow(non_snake_case)]
pub struct ZoneStateOpenWindowApiResponse {
    pub detectedTime: String, // RFC 3339 timestamp
    pub durationInSeconds: i32,
    pub expiry: String,
    pub remainingTimeInSeconds: i32,
}

#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct ZoneStateSettingApiResponse {
    #[serde(rename = "type")]
    pub deviceType: String,
    /// `ON` or `OFF`.
    #[serde(default)]
    pub power: Option<String>,
    pub temperature: Option<ZoneStateSettingTemperatureApiResponse>,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct ZoneStateSettingTemperatureApiResponse {
    pub celsius: f64,
    pub fahrenheit: f64,
}

#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct ZoneStateActivityDataPointsApiResponse {
    pub heatingPower: Option<ActivityDataPointsHeatingPowerApiResponse>,
    pub acPower: Option<ActivityDataPointsAcPowerApiResponse>,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct ActivityDataPointsHeatingPowerApiResponse {
    pub percentage: f64,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
pub struct ActivityDataPointsAcPowerApiResponse {
    pub value: String,
}

#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct ZoneStateSensorDataPointsApiResponse {
    pub insideTemperature: Option<SensorDataPointsInsideTemperatureApiResponse>,
    pub humidity: Option<SensorDataPointsHumidityApiResponse>,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct SensorDataPointsInsideTemperatureApiResponse {
    pub celsius: f64,
    pub fahrenheit: f64,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct SensorDataPointsHumidityApiResponse {
    pub percentage: f64,
}

#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct WeatherApiResponse {
    pub solarIntensity: WeatherSolarIntensityApiResponse,
    pub outsideTemperature: WeatherOutsideTemperatureApiResponse,
}
#[derive(Deserialize, Debug, PartialEq)]
pub struct WeatherSolarIntensityApiResponse {
    pub percentage: f64,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct WeatherOutsideTemperatureApiResponse {
    pub fahrenheit: f64,
    pub celsius: f64,
}

pub struct ZoneStateResponse {
    pub id: i32,
    pub name: String,
    pub state_response: ZoneStateApiResponse,
}

#[derive(Deserialize, Debug, PartialEq)]
#[allow(non_snake_case)]
pub struct MobileDeviceApiResponse {
    pub settings: MobileDeviceSettingsApiResponse,
    /// Absent when the phone has not shared its location.
    #[serde(default)]
    pub location: Option<MobileDeviceLocationApiResponse>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[allow(non_snake_case)]
pub struct MobileDeviceSettingsApiResponse {
    pub geoTrackingEnabled: bool,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[allow(non_snake_case)]
pub struct MobileDeviceLocationApiResponse {
    pub atHome: bool,
}
