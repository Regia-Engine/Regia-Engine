# `config/weather.ron` — the project's weather config (format v1)

Loader: `regia_vfx::weather::WeatherConfig::from_ron`. Optional, per project, at
`assets/config/weather.ron` (read as `config://weather.ron` through the `Vfs`, the same mount as
`config/input.ron`; packaged with the rest of `config/**`). A missing file means every default
below, and every effect off. Frozen like every content format (`docs/specs/12-assets.md` § 4a): a
newer `format_version` is refused with `weather_config_version` and keeps the defaults; a version 2
needs a migrator and `regia upgrade-project` support. Unknown fields are an error
(`weather_config_invalid`, naming the field), and so is a config preset whose name clashes with a
built-in preset. A file that fails is logged once, keeps the defaults, and leaves its error in
`render.get_weather`'s `config.error`. The validated config is not re-read during a run.

Every field has a default, so a file names only what it changes. Ranges are validated: every rate,
time and size is positive and finite, `snow_all_below_c < rain_all_above_c`, `gust_scale` and
`density_scale` are in `0..=4`.

`regia new` writes this file, with all three effects true, for `blank`, `first_person`,
`third_person` and `top_down`. `net_game` and `quick` ship no `assets/config/weather.ron`, so those
projects keep every effect off.

```ron
(
    format_version: 1,
    // Named weathers beside the built-in ones. Each starts from `base` (a built-in preset or an
    // earlier one of these) and replaces the given fields: coverage, cloud_type, density,
    // wind_m_per_s, precipitation, temperature_c, precipitation_kind, gustiness.
    presets: [
        (name: "monsoon", base: "storm", temperature_c: Some(27.0), precipitation: Some(1.0), gustiness: Some(0.5)),
    ],
    // Which effects run. Each is false when absent, so an existing project changes nothing.
    // blank, first_person, third_person and top_down write all three as true.
    effects: (precipitation: true, surface: true, scene_wind: true),
    // A preset to start in when a level opens without a Weather resource (None: leave the sky alone).
    start: None,
    precipitation: (
        max_rain_mm_per_h: 50.0,     // the rain rate at precipitation 1
        max_snow_we_mm_per_h: 5.0,   // the snow rate at precipitation 1, water equivalent
        snow_all_below_c: 0.0,
        rain_all_above_c: 2.5,
        snow_ratio: 10.0,            // fresh snow depth per depth of water
        visible_drop_mm: 1.0,        // only drops above this size are drawn
        density_scale: 1.0,          // multiplies the drawn counts, 0..4
        streak_exposure_s: 0.0167,   // the shutter streaks are stretched by
        splashes: true,
    ),
    wind: (
        mode: Weather,          // Weather | Manual | Off: the SceneWind mode the plugin inserts
        surface_factor: None,   // None: (10 m / cloud base)^(1/7)
        veer_deg: 0.0,          // turns the surface wind relative to the wind aloft
        gust_scale: 1.0,        // multiplies the weather's gustiness, 0..4
    ),
    surface: (
        wet_mm: 0.2,            // water that wets a surface fully
        dry_time_s: 1800.0,     // seconds to dry at a drying factor of 1
        drain_mm_per_h: 2.5,    // what the ground takes without pooling
        puddle_mm: 5.0,         // water that fills a hollow
        puddle_dry_time_s: 3600.0,
        full_cover_m: 0.03,     // snow depth that hides the ground
        melt_mm_per_c_day: 4.0, // degree-day melt, water
        melt_scale: 1.0,        // game time runs fast: a realistic melt takes days
    ),
    visibility: (couple_fog: true, rain_visibility_m: 3000.0, snow_visibility_m: 400.0),
)
```

The same fields are in `docs/formats/weather.example.ron`, which a test parses.

The JSON Schema is `docs/formats/weather.ron.schema.json` (editor completion and CI checks of
converted files). A test keeps its keys equal to the serde fields, both ways. It writes RON's bare
enums as strings and `None` as `null`.

Registered in `cargo xtask check-formats` as `weather` (`WEATHER_CONFIG_FORMAT_VERSION`, the serde
shape fingerprint `weather_config_schema_fingerprint`, no migration sources at v1). Migration note:
a new format, so there is nothing to migrate. `regia upgrade-project` knows the format, so a later
version can migrate it.
