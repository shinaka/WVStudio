"""Current weather from Open-Meteo: a wvplay plugin in poll mode, and the template for writing others.

The player runs this once per interval, in this folder, with the settings as JSON on stdin:
    {"settings": {"latitude": 40.7, "longitude": -74.0, "units": "celsius"}}
It prints one JSON object: {"values": {"weather.temp": 21.5, ...}} or {"error": "what went wrong"}.
Only keys under "weather." count (the plugin's id); values are numbers, text or ISO time strings.
"""

import json
import sys
from datetime import datetime, timezone
import urllib.parse
import urllib.request

API = "https://api.open-meteo.com/v1/forecast"

# WMO weather codes, as Open-Meteo reports them
DESCRIPTIONS = {
    0: "Clear", 1: "Mostly clear", 2: "Partly cloudy", 3: "Overcast",
    45: "Fog", 48: "Icy fog",
    51: "Light drizzle", 53: "Drizzle", 55: "Heavy drizzle", 56: "Freezing drizzle", 57: "Freezing drizzle",
    61: "Light rain", 63: "Rain", 65: "Heavy rain", 66: "Freezing rain", 67: "Freezing rain",
    71: "Light snow", 73: "Snow", 75: "Heavy snow", 77: "Snow grains",
    80: "Showers", 81: "Showers", 82: "Heavy showers", 85: "Snow showers", 86: "Heavy snow showers",
    95: "Thunderstorm", 96: "Thunderstorm with hail", 99: "Thunderstorm with hail",
}


def fetch(settings):
    """The current conditions at the settings' place, as values for the player."""
    units = settings.get("units") or "celsius"
    query = {
        "latitude": settings.get("latitude", 40.7),
        "longitude": settings.get("longitude", -74.0),
        "current": "temperature_2m,weather_code,wind_speed_10m,relative_humidity_2m",
        "temperature_unit": units,
        "wind_speed_unit": "mph" if units == "fahrenheit" else "kmh",
        "timeformat": "unixtime",
    }
    request = urllib.request.Request(f"{API}?{urllib.parse.urlencode(query)}", headers={"User-Agent": "wvplay"})
    with urllib.request.urlopen(request, timeout=15) as reply:
        current = json.load(reply)["current"]
    code = current.get("weather_code")
    values = {
        "weather.temp": current.get("temperature_2m"),
        "weather.desc": DESCRIPTIONS.get(code, f"Code {code}"),
        "weather.wind": current.get("wind_speed_10m"),
        "weather.humidity": current.get("relative_humidity_2m"),
        "weather.code": code,
        # when the readings were taken, as an ISO time, so {weather.at:%H:%M} works
        "weather.at": datetime.fromtimestamp(current["time"], timezone.utc).isoformat() if "time" in current else None,
    }
    return {k: v for k, v in values.items() if v is not None}


def main():
    settings = json.load(sys.stdin).get("settings", {})
    try:
        print(json.dumps({"values": fetch(settings)}))
    except Exception as e:  # anything at all: the player shows the message as the source's error
        print(json.dumps({"error": f"{type(e).__name__}: {e}"}))


if __name__ == "__main__":
    main()
