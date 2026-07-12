//! Weather service: geocoding, Open-Meteo / MET.no fetch, unit
//! conversion, and the terminal weather display.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

// ─── Weather data ──────────────────────────────────────────

pub(crate) struct ForecastDay {
    date: String,
    // Stored in metric (Celsius) and converted to the display unit at render
    // time, so a units toggle never needs to re-fetch.
    high_c: f64,
    low_c: f64,
    desc: String,
}

pub(crate) struct WeatherData {
    city: String,
    region: String,
    country: String,
    // Retained for "auto" units resolution (US -> imperial, else metric); not
    // displayed directly.
    country_code: String,
    // All values are metric as fetched; display converts per `WeatherUnits`.
    temp_c: f64,
    feels_like_c: f64,
    humidity: i64,
    wind_kmh: f64,
    wind_dir: String,
    desc: String,
    forecast: Vec<ForecastDay>,
}

/// One geocoding hit, parsed out of the Open-Meteo `results` array so the
/// selection logic can be unit-tested without any JSON or network.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GeoResult {
    pub(in crate::telnet) name: String,
    pub(in crate::telnet) admin1: String,
    pub(in crate::telnet) country: String,
    pub(in crate::telnet) country_code: String,
    pub(in crate::telnet) lat: f64,
    pub(in crate::telnet) lon: f64,
    pub(in crate::telnet) timezone: String,
}

/// Weather display units.  Resolved once per fetch from the `weather_units`
/// config setting and the geocoded country.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum WeatherUnits {
    Imperial,
    Metric,
}

impl WeatherUnits {
    pub(in crate::telnet) fn is_imperial(self) -> bool {
        self == WeatherUnits::Imperial
    }
    /// Short temperature unit label (`F` / `C`) — no degree sign, since
    /// PETSCII/ASCII terminals can't render one.
    pub(in crate::telnet) fn temp_label(self) -> &'static str {
        if self.is_imperial() { "F" } else { "C" }
    }
    /// Wind speed unit label (`mph` / `km/h`).
    pub(in crate::telnet) fn wind_label(self) -> &'static str {
        if self.is_imperial() { "mph" } else { "km/h" }
    }
}

/// Resolve display units from the `weather_units` setting and a geocoded
/// ISO-3166 country code.  `auto` (or any unrecognized value) uses Fahrenheit
/// for the US and Celsius everywhere else — matching each locale's norm while
/// preserving the historical US-only behavior.
pub(crate) fn resolve_weather_units(setting: &str, country_code: &str) -> WeatherUnits {
    match setting.trim().to_ascii_lowercase().as_str() {
        "us" => WeatherUnits::Imperial,
        "metric" => WeatherUnits::Metric,
        _ => {
            if country_code.eq_ignore_ascii_case("US") {
                WeatherUnits::Imperial
            } else {
                WeatherUnits::Metric
            }
        }
    }
}

/// Format a Celsius temperature for display in the chosen units (rounded, no
/// unit suffix — the caller appends the label).  Rounds to a whole integer so
/// a value in the (-0.5, 0) band shows as `0`, never `-0`.
pub(crate) fn format_temp(temp_c: f64, units: WeatherUnits) -> String {
    let v = if units.is_imperial() { temp_c * 9.0 / 5.0 + 32.0 } else { temp_c };
    format!("{}", v.round() as i64)
}

/// Format a km/h wind speed for display in the chosen units (rounded).
pub(crate) fn format_wind(wind_kmh: f64, units: WeatherUnits) -> String {
    let v = if units.is_imperial() { wind_kmh * 0.621_371 } else { wind_kmh };
    format!("{}", v.round() as i64)
}

/// Clean and validate a user-entered weather location.  Accepts city names and
/// postal codes worldwide (any script) — the only rejections are empty input
/// and absurdly long input.  Control characters are stripped so the value
/// can't corrupt the geocoder URL or the saved `egateway.conf` line.  Returns
/// the cleaned query, or a short error message for the terminal.
pub(crate) fn validate_weather_location(input: &str) -> Result<String, &'static str> {
    let cleaned: String = input.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Err("Enter a city or postal code.");
    }
    // Generous cap: real place/"city, country" queries are well under this;
    // the bound just stops a pathological paste from reaching the URL.
    if cleaned.chars().count() > 60 {
        return Err("Location too long.");
    }
    Ok(cleaned.to_string())
}

/// Split a location query into a geocoder search term and an optional
/// disambiguating qualifier, on the LAST comma.  `"London, GB"` ->
/// `("London", Some("GB"))`; `"62051"` -> `("62051", None)`.  A trailing or
/// leading empty side is ignored (treated as no qualifier).
pub(crate) fn split_location_query(query: &str) -> (String, Option<String>) {
    if let Some((name, qual)) = query.rsplit_once(',') {
        let name = name.trim();
        let qual = qual.trim();
        if !name.is_empty() && !qual.is_empty() {
            return (name.to_string(), Some(qual.to_string()));
        }
    }
    (query.trim().to_string(), None)
}

/// Choose a geocoding result.  Without a qualifier, take the first (Open-Meteo
/// ranks by prominence, so "London" -> UK, "Paris" -> France).  With a
/// qualifier, pick the first result whose country code, country name, or
/// region (`admin1`) matches it case-insensitively — enabling "London, GB",
/// "London, Ontario", "Paris, France", "Paris, Texas".  Returns `None` when a
/// qualifier is given but nothing matches (so the caller reports "not found"
/// rather than silently showing the wrong city).
pub(crate) fn pick_geo_result<'a>(results: &'a [GeoResult], qualifier: Option<&str>) -> Option<&'a GeoResult> {
    match qualifier {
        None => results.first(),
        Some(q) => {
            let q = q.trim();
            // Two passes so matching is deterministic when a qualifier is
            // ambiguous.  An exact country-code / country-name / region match
            // wins first (so "London, CA" resolves to Canada, not California);
            // only if nothing matches exactly do we expand a US state
            // abbreviation ("Springfield, IL" -> Illinois), which is why
            // "Paris, TX" still works.
            let exact = results.iter().find(|r| {
                r.country_code.eq_ignore_ascii_case(q)
                    || r.country.eq_ignore_ascii_case(q)
                    || r.admin1.eq_ignore_ascii_case(q)
            });
            if exact.is_some() {
                return exact;
            }
            expand_us_state(q)
                .and_then(|full| results.iter().find(|r| r.admin1.eq_ignore_ascii_case(full)))
        }
    }
}

/// Expand a USPS 2-letter state/territory code to the full state name
/// Open-Meteo reports in `admin1`.  Returns `None` for anything that isn't a
/// recognized code, so non-US qualifiers fall through to the other matchers.
pub(crate) fn expand_us_state(code: &str) -> Option<&'static str> {
    let full = match code.to_ascii_uppercase().as_str() {
        "AL" => "Alabama",
        "AK" => "Alaska",
        "AZ" => "Arizona",
        "AR" => "Arkansas",
        "CA" => "California",
        "CO" => "Colorado",
        "CT" => "Connecticut",
        "DE" => "Delaware",
        "FL" => "Florida",
        "GA" => "Georgia",
        "HI" => "Hawaii",
        "ID" => "Idaho",
        "IL" => "Illinois",
        "IN" => "Indiana",
        "IA" => "Iowa",
        "KS" => "Kansas",
        "KY" => "Kentucky",
        "LA" => "Louisiana",
        "ME" => "Maine",
        "MD" => "Maryland",
        "MA" => "Massachusetts",
        "MI" => "Michigan",
        "MN" => "Minnesota",
        "MS" => "Mississippi",
        "MO" => "Missouri",
        "MT" => "Montana",
        "NE" => "Nebraska",
        "NV" => "Nevada",
        "NH" => "New Hampshire",
        "NJ" => "New Jersey",
        "NM" => "New Mexico",
        "NY" => "New York",
        "NC" => "North Carolina",
        "ND" => "North Dakota",
        "OH" => "Ohio",
        "OK" => "Oklahoma",
        "OR" => "Oregon",
        "PA" => "Pennsylvania",
        "RI" => "Rhode Island",
        "SC" => "South Carolina",
        "SD" => "South Dakota",
        "TN" => "Tennessee",
        "TX" => "Texas",
        "UT" => "Utah",
        "VT" => "Vermont",
        "VA" => "Virginia",
        "WA" => "Washington",
        "WV" => "West Virginia",
        "WI" => "Wisconsin",
        "WY" => "Wyoming",
        _ => return None,
    };
    Some(full)
}

/// Parse the Open-Meteo geocoder JSON `results` array into `GeoResult`s,
/// skipping any entry missing coordinates.
pub(crate) fn parse_geo_results(v: &serde_json::Value) -> Vec<GeoResult> {
    let Some(arr) = v.get("results").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|r| {
            let lat = r.get("latitude").and_then(|v| v.as_f64())?;
            let lon = r.get("longitude").and_then(|v| v.as_f64())?;
            let s = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some(GeoResult {
                name: s("name"),
                admin1: s("admin1"),
                country: s("country"),
                country_code: s("country_code"),
                lat,
                lon,
                timezone: {
                    let tz = s("timezone");
                    if tz.is_empty() { "auto".to_string() } else { tz }
                },
            })
        })
        .collect()
}

impl TelnetSession {
    // ─── WEATHER ────────────────────────────────────────────

    pub(in crate::telnet) async fn weather(&mut self) -> Result<(), std::io::Error> {
        let saved_location = self.weather_location.clone();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("WEATHER")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        // Prompt for a location (city or postal code, worldwide) with default.
        self.send_line(&format!(
            "  {}",
            self.dim("City or postal code, e.g. London, GB")
        ))
        .await?;
        if saved_location.is_empty() {
            self.send(&format!("  {}: ", self.cyan("Location")))
                .await?;
        } else {
            self.send(&format!(
                "  {} [{}]: ",
                self.cyan("Location"),
                self.amber(&saved_location)
            ))
            .await?;
        }
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) => s,
            None => return Ok(()),
        };

        let location = if input.trim().is_empty() {
            if saved_location.is_empty() {
                return Ok(());
            }
            saved_location
        } else {
            match validate_weather_location(&input) {
                Ok(loc) => loc,
                Err(msg) => {
                    self.show_error(msg).await?;
                    return Ok(());
                }
            }
        };

        self.send_line("").await?;
        self.send_line(&format!("  {}...", self.dim("Loading")))
            .await?;
        self.flush().await?;

        // Save the location for next time (session + config file).
        self.weather_location = location.clone();
        let loc_for_save = location.clone();
        tokio::task::spawn_blocking(move || {
            config::update_config_value("weather_location", &loc_for_save);
        })
        .await
        .ok();

        // Fetch weather from Open-Meteo (free, no API key). Always fetched in
        // metric; the display converts to the user's units.
        let loc_owned = location.clone();
        let result = tokio::task::spawn_blocking(move || {
            Self::fetch_weather(&loc_owned)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        match result {
            Ok(weather) => {
                self.display_weather(&weather).await?;
            }
            Err(e) => {
                let max_w = if self.terminal_type == TerminalType::Petscii { 30 } else { 50 };
                let safe = crate::aichat::sanitize_for_terminal(&e);
                self.show_error(&truncate_to_width(&safe, max_w)).await?;
            }
        }
        Ok(())
    }

    /// GET a URL and read the size-capped body, retrying once on a transient
    /// transport failure (connect drop, reset).  Returns the body bytes or a
    /// short error string.
    pub(in crate::telnet) fn weather_http_get(agent: &ureq::Agent, url: &str, cap: u64) -> Result<Vec<u8>, String> {
        // MET Norway (the fallback provider) rejects requests without a
        // descriptive User-Agent (HTTP 403); Open-Meteo accepts it too, so set
        // it on every weather call.
        const UA: &str = concat!(
            "ethernet-gateway/",
            env!("CARGO_PKG_VERSION"),
            " (https://github.com/rickybryce/ethernet-gateway)"
        );
        let mut last = String::new();
        for _ in 0..2 {
            match agent.get(url).header("User-Agent", UA).call() {
                Ok(resp) => {
                    let mut bytes = Vec::new();
                    resp.into_body()
                        .as_reader()
                        .take(cap)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    return Ok(bytes);
                }
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    }

    pub(in crate::telnet) fn fetch_weather(location: &str) -> Result<WeatherData, String> {
        // Clean/bound the query here too — not just at the interactive prompt —
        // so a value loaded from egateway.conf (hand-edited, or a legacy
        // migration) can't reach the geocoder URL unchecked.
        let location = validate_weather_location(location).map_err(|e| e.to_string())?;

        // Short connect timeout so an unreachable/blocked host fails fast (don't
        // leave the menu hanging on a dead endpoint), with a modest overall cap.
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_connect(Some(std::time::Duration::from_secs(5)))
                .timeout_global(Some(std::time::Duration::from_secs(12)))
                .build(),
        );

        // Step 1: Geocode the location via Open-Meteo.  Split off an optional
        // "City, Qualifier" so a common name can be disambiguated by country or
        // region; percent-encode the search term (city names / postal codes may
        // contain spaces or non-ASCII).  Fetch several candidates so the
        // qualifier can select among them.
        let (name, qualifier) = split_location_query(&location);
        let geo_url = format!(
            "https://geocoding-api.open-meteo.com/v1/search?name={}&count=25&language=en&format=json",
            crate::webserver::encode_query(&name)
        );
        let geo_bytes = Self::weather_http_get(&agent, &geo_url, 128 * 1024)
            .map_err(|_| "Weather service unreachable. Try again later.".to_string())?;
        let geo: serde_json::Value = serde_json::from_slice(&geo_bytes)
            .map_err(|_| "Weather service returned bad data.".to_string())?;

        let results = parse_geo_results(&geo);
        let result = pick_geo_result(&results, qualifier.as_deref())
            .ok_or("Not found - try 'City, Country'.")?
            .clone();
        let lat = result.lat;
        let lon = result.lon;

        // Step 2: Fetch weather from Open-Meteo, always in metric — the display
        // layer converts to the user's units, so a units change never re-fetches.
        let wx_url = format!(
            "https://api.open-meteo.com/v1/forecast?\
             latitude={}&longitude={}\
             &current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m,wind_direction_10m\
             &daily=weather_code,temperature_2m_max,temperature_2m_min\
             &temperature_unit=celsius&wind_speed_unit=kmh\
             &timezone={}&forecast_days=3",
            lat, lon, crate::webserver::encode_query(&result.timezone)
        );
        let wx_bytes = match Self::weather_http_get(&agent, &wx_url, 128 * 1024) {
            Ok(b) => b,
            // Primary forecast host unreachable — fall back to MET Norway,
            // reusing the coordinates we already geocoded via Open-Meteo.
            Err(_) => return Self::fetch_weather_metno(&agent, &result),
        };
        let wx: serde_json::Value = serde_json::from_slice(&wx_bytes)
            .map_err(|_| "Weather service returned bad data.".to_string())?;

        let current = wx.get("current").ok_or("No current weather")?;
        let temp_c = current.get("temperature_2m").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let feels_like_c = current.get("apparent_temperature").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let humidity = current.get("relative_humidity_2m").and_then(|v| v.as_i64()).unwrap_or(0);
        let wind_kmh = current.get("wind_speed_10m").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let wind_deg = current.get("wind_direction_10m").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let weather_code = current.get("weather_code").and_then(|v| v.as_i64()).unwrap_or(-1);

        let wind_dir = Self::degrees_to_compass(wind_deg);
        let desc = Self::wmo_weather_description(weather_code);

        // Extract 3-day forecast (Celsius; converted at display time).
        let mut forecast = Vec::new();
        if let Some(daily) = wx.get("daily") {
            let dates = daily.get("time").and_then(|v| v.as_array());
            let highs = daily.get("temperature_2m_max").and_then(|v| v.as_array());
            let lows = daily.get("temperature_2m_min").and_then(|v| v.as_array());
            let codes = daily.get("weather_code").and_then(|v| v.as_array());
            if let (Some(dates), Some(highs), Some(lows), Some(codes)) = (dates, highs, lows, codes) {
                for (i, date_v) in dates.iter().enumerate().take(3) {
                    // Index the sibling arrays defensively via `.get(i)` — a
                    // malformed upstream response can return a `time` array
                    // longer than the value arrays, and a bare `highs[i]` would
                    // then panic (the rest of this parser is fault-tolerant).
                    let date = date_v.as_str().unwrap_or("?").to_string();
                    let high_c = highs.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let low_c = lows.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let code = codes.get(i).and_then(|v| v.as_i64()).unwrap_or(-1);
                    forecast.push(ForecastDay {
                        date,
                        high_c,
                        low_c,
                        desc: Self::wmo_weather_description(code).to_string(),
                    });
                }
            }
        }

        Ok(WeatherData {
            city: result.name.clone(),
            region: result.admin1.clone(),
            country: result.country.clone(),
            country_code: result.country_code.clone(),
            temp_c,
            feels_like_c,
            humidity,
            wind_kmh,
            wind_dir: wind_dir.to_string(),
            desc: desc.to_string(),
            forecast,
        })
    }

    /// Fallback forecast provider: MET Norway (api.met.no) Locationforecast
    /// 2.0 — free, no API key (needs the descriptive User-Agent set in
    /// `weather_http_get`).  Reuses the `GeoResult` already geocoded via
    /// Open-Meteo (worldwide coverage, so this works for any location, not just
    /// the US) and keeps MET's native Celsius / km-h into the metric
    /// `WeatherData` the display layer converts.  Daily high/low are aggregated
    /// by UTC date (a fallback approximation — the primary Open-Meteo path uses
    /// the location's local day boundaries).
    pub(in crate::telnet) fn fetch_weather_metno(
        agent: &ureq::Agent,
        geo: &GeoResult,
    ) -> Result<WeatherData, String> {
        let url = format!(
            "https://api.met.no/weatherapi/locationforecast/2.0/compact?lat={:.4}&lon={:.4}",
            geo.lat, geo.lon
        );
        let bytes = Self::weather_http_get(agent, &url, 512 * 1024)
            .map_err(|_| "Weather service unreachable. Try again later.".to_string())?;
        let doc: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Weather service returned bad data.".to_string())?;

        let series = doc
            .get("properties")
            .and_then(|p| p.get("timeseries"))
            .and_then(|t| t.as_array())
            .ok_or("Weather service returned bad data.")?;
        let first = series.first().ok_or("No current weather")?;
        let inst = first
            .get("data")
            .and_then(|d| d.get("instant"))
            .and_then(|i| i.get("details"))
            .ok_or("No current weather")?;

        let ms_to_kmh = |ms: f64| ms * 3.6;

        let temp_c = inst.get("air_temperature").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let humidity = inst.get("relative_humidity").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let wind_ms = inst.get("wind_speed").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let wind_deg = inst.get("wind_from_direction").and_then(|v| v.as_f64()).unwrap_or(0.0);

        // Current condition: prefer the next-hour symbol, else the 6-hour one.
        let desc = first
            .get("data")
            .and_then(|d| d.get("next_1_hours").or_else(|| d.get("next_6_hours")))
            .and_then(|n| n.get("summary"))
            .and_then(|s| s.get("symbol_code"))
            .and_then(|v| v.as_str())
            .map(Self::metno_symbol_description)
            .unwrap_or_else(|| "Unknown".to_string());

        // 3-day forecast: group instant air temps by UTC date → min/max, and
        // take the first symbol seen for each day.
        let mut days: std::collections::BTreeMap<String, (f64, f64, Option<String>)> =
            std::collections::BTreeMap::new();
        for entry in series {
            let time = match entry.get("time").and_then(|v| v.as_str()) {
                Some(t) if t.len() >= 10 => t,
                _ => continue,
            };
            let date = time[..10].to_string();
            let Some(data) = entry.get("data") else { continue };
            let Some(t) = data
                .get("instant")
                .and_then(|i| i.get("details"))
                .and_then(|d| d.get("air_temperature"))
                .and_then(|v| v.as_f64())
            else {
                continue;
            };
            let e = days.entry(date).or_insert((t, t, None));
            if t < e.0 {
                e.0 = t;
            }
            if t > e.1 {
                e.1 = t;
            }
            if e.2.is_none() {
                if let Some(sym) = data
                    .get("next_6_hours")
                    .or_else(|| data.get("next_1_hours"))
                    .and_then(|n| n.get("summary"))
                    .and_then(|s| s.get("symbol_code"))
                    .and_then(|v| v.as_str())
                {
                    e.2 = Some(sym.to_string());
                }
            }
        }
        let forecast: Vec<ForecastDay> = days
            .into_iter()
            .take(3)
            .map(|(date, (lo, hi, sym))| ForecastDay {
                date,
                high_c: hi,
                low_c: lo,
                desc: sym
                    .map(|s| Self::metno_symbol_description(&s))
                    .unwrap_or_else(|| "Unknown".to_string()),
            })
            .collect();

        Ok(WeatherData {
            city: geo.name.clone(),
            region: geo.admin1.clone(),
            country: geo.country.clone(),
            country_code: geo.country_code.clone(),
            temp_c,
            // MET's compact product has no apparent temperature; use air temp.
            feels_like_c: temp_c,
            humidity: humidity as i64,
            wind_kmh: ms_to_kmh(wind_ms),
            wind_dir: Self::degrees_to_compass(wind_deg).to_string(),
            desc,
            forecast,
        })
    }

    /// Map a MET Norway `symbol_code` (e.g. `partlycloudy_day`) to a short
    /// description.  The `_day` / `_night` / `_polartwilight` variant suffix is
    /// dropped; unknown codes fall back to the raw base code.
    pub(in crate::telnet) fn metno_symbol_description(code: &str) -> String {
        let base = code.split('_').next().unwrap_or(code);
        let d = match base {
            "clearsky" => "Clear sky",
            "fair" => "Fair",
            "partlycloudy" => "Partly cloudy",
            "cloudy" => "Cloudy",
            "fog" => "Fog",
            "rain" => "Rain",
            "lightrain" => "Light rain",
            "heavyrain" => "Heavy rain",
            "rainandthunder" => "Rain and thunder",
            "lightrainandthunder" => "Light rain, thunder",
            "heavyrainandthunder" => "Heavy rain, thunder",
            "rainshowers" => "Rain showers",
            "lightrainshowers" => "Light rain showers",
            "heavyrainshowers" => "Heavy rain showers",
            "rainshowersandthunder" => "Rain showers, thunder",
            "lightrainshowersandthunder" => "Light rain showers, thunder",
            "heavyrainshowersandthunder" => "Heavy rain showers, thunder",
            "sleet" => "Sleet",
            "lightsleet" => "Light sleet",
            "heavysleet" => "Heavy sleet",
            "sleetandthunder" => "Sleet and thunder",
            "lightsleetandthunder" => "Light sleet, thunder",
            "heavysleetandthunder" => "Heavy sleet, thunder",
            "sleetshowers" => "Sleet showers",
            "lightsleetshowers" => "Light sleet showers",
            "heavysleetshowers" => "Heavy sleet showers",
            "sleetshowersandthunder" => "Sleet showers, thunder",
            "lightssleetshowersandthunder" => "Light sleet showers, thunder",
            "heavysleetshowersandthunder" => "Heavy sleet showers, thunder",
            "snow" => "Snow",
            "lightsnow" => "Light snow",
            "heavysnow" => "Heavy snow",
            "snowandthunder" => "Snow and thunder",
            "lightsnowandthunder" => "Light snow, thunder",
            "heavysnowandthunder" => "Heavy snow, thunder",
            "snowshowers" => "Snow showers",
            "lightsnowshowers" => "Light snow showers",
            "heavysnowshowers" => "Heavy snow showers",
            "snowshowersandthunder" => "Snow showers, thunder",
            "lightssnowshowersandthunder" => "Light snow showers, thunder",
            "heavysnowshowersandthunder" => "Heavy snow showers, thunder",
            _ => "",
        };
        if d.is_empty() {
            base.to_string()
        } else {
            d.to_string()
        }
    }

    pub(in crate::telnet) fn degrees_to_compass(deg: f64) -> &'static str {
        const DIRS: [&str; 16] = [
            "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
            "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
        ];
        let idx = ((deg + 11.25) / 22.5) as usize % 16;
        DIRS[idx]
    }

    pub(in crate::telnet) fn wmo_weather_description(code: i64) -> &'static str {
        match code {
            0 => "Clear sky",
            1 => "Mainly clear",
            2 => "Partly cloudy",
            3 => "Overcast",
            45 => "Fog",
            48 => "Depositing rime fog",
            51 => "Light drizzle",
            53 => "Moderate drizzle",
            55 => "Dense drizzle",
            56 => "Light freezing drizzle",
            57 => "Dense freezing drizzle",
            61 => "Slight rain",
            63 => "Moderate rain",
            65 => "Heavy rain",
            66 => "Light freezing rain",
            67 => "Heavy freezing rain",
            71 => "Slight snow",
            73 => "Moderate snow",
            75 => "Heavy snow",
            77 => "Snow grains",
            80 => "Slight rain showers",
            81 => "Moderate rain showers",
            82 => "Violent rain showers",
            85 => "Slight snow showers",
            86 => "Heavy snow showers",
            95 => "Thunderstorm",
            96 => "Thunderstorm, slight hail",
            99 => "Thunderstorm, heavy hail",
            _ => "Unknown",
        }
    }

    pub(in crate::telnet) async fn display_weather(
        &mut self,
        w: &WeatherData,
    ) -> Result<(), std::io::Error> {
        // Render in a loop so 'U' can toggle the display units in place —
        // the data is stored in metric, so switching never needs a re-fetch.
        loop {
            let units_setting = config::get_config().weather_units;
            let units = resolve_weather_units(&units_setting, &w.country_code);
            let tl = units.temp_label();

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;

            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let max_loc = if is_petscii { 30 } else { 48 };
            // City, region, and country — so the user can tell which match they
            // got (e.g. London GB vs London CA vs London US).
            let location = [w.city.as_str(), w.region.as_str(), w.country.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            let loc_display = truncate_to_width(&location, max_loc);
            self.send_line(&format!("  {}", self.yellow(&loc_display)))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            // Current conditions
            let max_desc = if is_petscii { 26 } else { 40 };
            self.send_line(&format!(
                "  Current: {}",
                self.white(&truncate_to_width(&w.desc, max_desc))
            ))
            .await?;
            self.send_line(&format!(
                "  Temp: {}{} (Feels like {}{})",
                self.white(&format_temp(w.temp_c, units)),
                tl,
                self.white(&format_temp(w.feels_like_c, units)),
                tl,
            ))
            .await?;
            self.send_line(&format!(
                "  Humidity: {}%",
                self.white(&w.humidity.to_string())
            ))
            .await?;
            self.send_line(&format!(
                "  Wind: {} {} {}",
                self.white(&w.wind_dir),
                self.white(&format_wind(w.wind_kmh, units)),
                units.wind_label(),
            ))
            .await?;
            self.send_line("").await?;

            // Forecast
            if !w.forecast.is_empty() {
                self.send_line(&format!("  {}", self.yellow("Forecast:")))
                    .await?;
                for (i, day) in w.forecast.iter().enumerate() {
                    let label = match i {
                        0 => "Today",
                        1 => "Tomorrow",
                        _ => &day.date,
                    };
                    let max_fd = if is_petscii { 12 } else { 20 };
                    let desc_part = if day.desc.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", truncate_to_width(&day.desc, max_fd))
                    };
                    self.send_line(&format!(
                        "  {}: {}{} / {}{}{}",
                        self.cyan(label),
                        format_temp(day.high_c, units),
                        tl,
                        format_temp(day.low_c, units),
                        tl,
                        desc_part,
                    ))
                    .await?;
                }
            }

            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.dim(&format!("Units: {}", units_setting))
            ))
            .await?;
            self.send_line("").await?;
            self.send("  U=change units   other key=back").await?;
            self.flush().await?;

            let key = self.wait_for_key_returning().await?;
            if key == b'u' || key == b'U' {
                // Cycle auto -> us -> metric -> auto and persist, then re-render.
                let next = match units_setting.as_str() {
                    "auto" => "us",
                    "us" => "metric",
                    _ => "auto",
                }
                .to_string();
                tokio::task::spawn_blocking(move || {
                    config::update_config_value("weather_units", &next);
                })
                .await
                .ok();
                continue;
            }
            return Ok(());
        }
    }
}
