use std::convert::Infallible;

use super::model::{WeatherApiResponse, ZoneStateResponse};
use super::rate_limit::RateLimit;

use hyper::{header::CONTENT_TYPE, Body, Request, Response};
use lazy_static::lazy_static;
use log::info;
use prometheus::{CounterVec, Encoder, GaugeVec, TextEncoder};

lazy_static! {
    pub static ref ACTIVITY_HEATING_POWER: GaugeVec = register_gauge_vec!(
        "tado_activity_heating_power_percentage",
        "The % of heating power in a specific zone.",
        &["zone", "type"]
    )
    .unwrap();
    pub static ref ACTIVITY_AC_POWER: GaugeVec = register_gauge_vec!(
        "tado_activity_ac_power_value",
        "The value of ac power in a specific zone.",
        &["zone", "type"]
    )
    .unwrap();
    pub static ref SETTING_TEMPERATURE: GaugeVec = register_gauge_vec!(
        "tado_setting_temperature_value",
        "The temperature of a specific zone in celsius degres.",
        &["zone", "type", "unit"]
    )
    .unwrap();
    pub static ref SENSOR_TEMPERATURE: GaugeVec = register_gauge_vec!(
        "tado_sensor_temperature_value",
        "The temperature of a specific zone in celsius degres.",
        &["zone", "type", "unit"]
    )
    .unwrap();
    pub static ref SENSOR_HUMIDITY_PERCENTAGE: GaugeVec = register_gauge_vec!(
        "tado_sensor_humidity_percentage",
        "The % of humidity in a specific zone.",
        &["zone", "type"]
    )
    .unwrap();
    pub static ref WEATHER_SOLAR_INTENSITY: GaugeVec = register_gauge_vec!(
        "weather_solar_intensity",
        "Solar intensity outside the house.",
        &[]
    )
    .unwrap();
    pub static ref WEATHER_OUTSIDE_TEMPERATURE: GaugeVec = register_gauge_vec!(
        "weather_outside_temperature",
        "Temperature outside the house.",
        &["unit"]
    )
    .unwrap();
    pub static ref SENSOR_WINDOW_OPENED: GaugeVec = register_gauge_vec!(
        "tado_sensor_window_opened",
        "1 if the sensor detected a window is open, 0 otherwise.",
        &["zone", "type"]
    )
    .unwrap();
    pub static ref AUTOMATION_ACTIONS: CounterVec = register_counter_vec!(
        "tado_automation_actions_total",
        "Automation decisions by rule, action and mode (dry_run, applied, failed).",
        &["rule", "action", "mode"]
    )
    .unwrap();
    pub static ref API_RATELIMIT_REMAINING: GaugeVec = register_gauge_vec!(
        "tado_api_ratelimit_remaining",
        "Requests left in the current tado° API quota window.",
        &[]
    )
    .unwrap();
    pub static ref API_RATELIMIT_QUOTA: GaugeVec = register_gauge_vec!(
        "tado_api_ratelimit_quota",
        "Requests allowed per tado° API quota window.",
        &[]
    )
    .unwrap();
    pub static ref API_RATELIMIT_RESET_SECONDS: GaugeVec = register_gauge_vec!(
        "tado_api_ratelimit_reset_seconds",
        "Seconds until the exhausted tado° API quota refills, 0 while requests remain.",
        &[]
    )
    .unwrap();
}

pub fn set_zones(zones: &[ZoneStateResponse]) {
    for zone in zones {
        let device_type: String = zone.state_response.setting.deviceType.clone();

        // The setting temperature may be null in the API response, if the
        // zone's heating mode is turned off. If the temperature setting is
        // absent, from the API response we'll simply not set its gauge values.
        if let Some(setting_temperature) = &zone.state_response.setting.temperature {
            // setting temperature
            let value: f64 = setting_temperature.celsius;
            SETTING_TEMPERATURE
                .with_label_values(&[zone.name.as_str(), device_type.as_str(), "celsius"])
                .set(value);
            info!(
                "-> {} ({}) -> setting temperature (celsius): {}",
                zone.name,
                device_type.as_str(),
                value
            );

            let value: f64 = setting_temperature.fahrenheit;
            SETTING_TEMPERATURE
                .with_label_values(&[zone.name.as_str(), device_type.as_str(), "fahrenheit"])
                .set(value);
            info!(
                "-> {} ({}) -> setting temperature (fahrenheit): {}",
                zone.name,
                device_type.as_str(),
                value
            );
        } else {
            info!(
                "-> {} ({}) -> setting temperature (celsius): Off",
                zone.name,
                device_type.as_str()
            );
            info!(
                "-> {} ({}) -> setting temperature (fahrenheit): Off",
                zone.name,
                device_type.as_str()
            );
        }

        // If openWindowDetected is not None, this means that a window is open.
        if zone.state_response.openWindow.is_some() {
            info!(
                "-> {} ({}) -> window opened: {}",
                zone.name,
                device_type.as_str(),
                true
            );
            SENSOR_WINDOW_OPENED
                .with_label_values(&[zone.name.as_str(), device_type.as_str()])
                .set(1.0);
        } else {
            info!(
                "-> {} ({}) -> window opened: {}",
                zone.name,
                device_type.as_str(),
                false
            );
            SENSOR_WINDOW_OPENED
                .with_label_values(&[zone.name.as_str(), device_type.as_str()])
                .set(0.0);
        }

        // sensor temperature
        if let Some(inside_temperature) = &zone.state_response.sensorDataPoints.insideTemperature {
            // celsius
            let value: f64 = inside_temperature.celsius;
            SENSOR_TEMPERATURE
                .with_label_values(&[zone.name.as_str(), device_type.as_str(), "celsius"])
                .set(value);
            info!(
                "-> {} ({}) -> sensor temperature (celsius): {}",
                zone.name,
                device_type.as_str(),
                value
            );

            // fahrenheit
            let value: f64 = inside_temperature.fahrenheit;
            SENSOR_TEMPERATURE
                .with_label_values(&[zone.name.as_str(), device_type.as_str(), "fahrenheit"])
                .set(value);
            info!(
                "-> {} ({}) -> sensor temperature (fahrenheit): {}",
                zone.name,
                device_type.as_str(),
                value
            );
        }

        // sensor humidity
        if let Some(humidity) = &zone.state_response.sensorDataPoints.humidity {
            let value: f64 = humidity.percentage;
            SENSOR_HUMIDITY_PERCENTAGE
                .with_label_values(&[zone.name.as_str(), device_type.as_str()])
                .set(value);
            info!(
                "-> {} ({}) -> sensor humidity: {}%",
                zone.name,
                device_type.as_str(),
                value
            );
        }

        // heating power
        if let Some(heating_power) = &zone.state_response.activityDataPoints.heatingPower {
            let value: f64 = heating_power.percentage;
            ACTIVITY_HEATING_POWER
                .with_label_values(&[zone.name.as_str(), device_type.as_str()])
                .set(value);
            info!(
                "-> {} ({}) -> heating power: {}%",
                zone.name,
                device_type.as_str(),
                value
            );
        }

        // ac power
        if let Some(ac_power) = &zone.state_response.activityDataPoints.acPower {
            let value: f64 = match ac_power.value.as_str() {
                "ON" => 1.0,
                "OFF" => 0.0,
                _ => 0.0,
            };

            ACTIVITY_AC_POWER
                .with_label_values(&[zone.name.as_str(), device_type.as_str()])
                .set(value);
            info!(
                "-> {} ({}) -> ac power: {}",
                zone.name,
                device_type.as_str(),
                value
            );
        }
    }
}

pub fn set_weather(weather_response: Option<&WeatherApiResponse>) {
    if let Some(weather) = weather_response {
        // setting solar intensity
        let solar_intensity_percentage = weather.solarIntensity.percentage;

        WEATHER_SOLAR_INTENSITY
            .with_label_values(&[])
            .set(weather.solarIntensity.percentage);
        info!(
            "-> setting solar intensity (percentage): {}",
            solar_intensity_percentage
        );

        // setting outside temperature
        let outside_temperature_celsius = weather.outsideTemperature.celsius;
        let outside_temperature_fahrenheit = weather.outsideTemperature.fahrenheit;

        WEATHER_OUTSIDE_TEMPERATURE
            .with_label_values(&["celsius"])
            .set(outside_temperature_celsius);
        info!(
            "-> setting outside temperature (celsius): {}",
            outside_temperature_celsius
        );

        WEATHER_OUTSIDE_TEMPERATURE
            .with_label_values(&["fahrenheit"])
            .set(outside_temperature_fahrenheit);
        info!(
            "-> setting outside temperature (fahrenheit): {}",
            outside_temperature_fahrenheit
        );
    }
}

pub fn set_rate_limit(rate_limit: Option<&RateLimit>) {
    let limit = match rate_limit {
        Some(limit) => limit,
        None => return,
    };

    if let Some(remaining) = limit.remaining {
        API_RATELIMIT_REMAINING
            .with_label_values(&[])
            .set(remaining as f64);
        info!("-> tado° API requests remaining today: {}", remaining);
    }

    if let Some(quota) = limit.quota {
        API_RATELIMIT_QUOTA.with_label_values(&[]).set(quota as f64);
    }

    API_RATELIMIT_RESET_SECONDS
        .with_label_values(&[])
        .set(limit.reset_seconds.unwrap_or(0) as f64);
}

pub fn record_automation_action(rule: &str, action: &str, mode: &str) {
    AUTOMATION_ACTIONS
        .with_label_values(&[rule, action, mode])
        .inc();
}

pub async fn renderer(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let metrics = prometheus::gather();
    let mut buffer = vec![];

    let encoder = TextEncoder::new();
    encoder.encode(&metrics, &mut buffer).unwrap();

    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(Body::from(buffer))
        .unwrap();

    Ok(response)
}

#[cfg(test)]
mod tests {
    use crate::tado::model::{
        WeatherOutsideTemperatureApiResponse, WeatherSolarIntensityApiResponse,
    };

    use super::*;

    use std::sync::Mutex;

    /// Serializes tests that write the weather gauges, so snapshots are not interleaved.
    static WEATHER_LOCK: Mutex<()> = Mutex::new(());

    // The Prometheus registry is global and shared by all tests in this binary, so tests look
    // metrics up by name instead of asserting on the whole registry.
    fn gauge_values(name: &str) -> Vec<(Vec<(String, String)>, f64)> {
        prometheus::gather()
            .into_iter()
            .filter(|family| family.get_name() == name)
            .flat_map(|family| family.get_metric().to_vec())
            .map(|metric| {
                let labels = metric
                    .get_label()
                    .iter()
                    .map(|l| (l.get_name().to_string(), l.get_value().to_string()))
                    .collect();
                (labels, metric.get_gauge().get_value())
            })
            .collect()
    }

    fn unit(value: &str) -> Vec<(String, String)> {
        vec![("unit".to_string(), value.to_string())]
    }

    #[test]
    fn test_set_weather_some() {
        // Arrange
        let _guard = WEATHER_LOCK.lock().unwrap();
        let weather_response = WeatherApiResponse {
            solarIntensity: WeatherSolarIntensityApiResponse { percentage: 100.0 },
            outsideTemperature: WeatherOutsideTemperatureApiResponse {
                celsius: 20.0,
                fahrenheit: 68.0,
            },
        };

        // Act
        set_weather(Some(&weather_response));

        // Assert
        assert_eq!(
            gauge_values("weather_outside_temperature"),
            vec![(unit("celsius"), 20.0), (unit("fahrenheit"), 68.0)]
        );
        assert_eq!(
            gauge_values("weather_solar_intensity"),
            vec![(vec![], 100.0)]
        );
    }

    #[test]
    fn test_set_weather_none_leaves_metrics_untouched() {
        // Arrange
        let _guard = WEATHER_LOCK.lock().unwrap();
        let before = gauge_values("weather_outside_temperature");

        // Act
        set_weather(None);

        // Assert
        assert_eq!(gauge_values("weather_outside_temperature"), before);
    }

    #[test]
    fn test_set_rate_limit() {
        // Arrange
        let exhausted = RateLimit {
            remaining: Some(0),
            reset_seconds: Some(600),
            quota: Some(100),
        };
        let refilled = RateLimit {
            remaining: Some(99),
            reset_seconds: None,
            quota: None,
        };

        // Act
        set_rate_limit(Some(&exhausted));
        set_rate_limit(Some(&refilled));
        set_rate_limit(None);

        // Assert
        assert_eq!(
            gauge_values("tado_api_ratelimit_remaining"),
            vec![(vec![], 99.0)]
        );
        assert_eq!(
            gauge_values("tado_api_ratelimit_quota"),
            vec![(vec![], 100.0)]
        );
        assert_eq!(
            gauge_values("tado_api_ratelimit_reset_seconds"),
            vec![(vec![], 0.0)]
        );
    }
}
