# Orca 2026.31.1 — how the weather/tide overlay is built (from the Hermes bundle)

React Native + Hermes. Evidence = constant names in the string table.

## Shape
A draggable **bottom sheet** over the map:
  BOTTOM_SHEET_BORDER_RADIUS, BOTTOM_SHEET_HEADER_HEIGHT, BOTTOM_SHEET_TOP_PADDING,
  animatedSheetHeight, animatedSheetState, bottomSheetSnapPoint, WEATHER_SHEET_REGION_ID

Inside it, one **lane ("canvas") per parameter**, all sharing one horizontal time axis:
  WIND_CANVAS_HEIGHT   / WIND_COLUMN_WIDTH   / WIND_COLOR_SPECTRUM   / WIND_LABELS
  WAVES_CANVAS_HEIGHT  / WAVES_SNAP_INTERVAL / WAVES_COLOR_SPECTRUM  / WAVES_LABELS
  CURRENTS_CANVAS_HEIGHT / CURRENTS_COLUMN_WIDTH / CURRENTS_COLOR_SPECTRUM / CURRENT_LABELS
  RAIN_CANVAS_HEIGHT   / RAIN_COLUMN_WIDTH   / RAIN_COLOR_SPECTRUM   / RAIN_LABELS
  TIDES_CANVAS_HEIGHT  / TIDES_SNAP_INTERVAL / TIDES_INDICATOR_RADIUS
    + TIDES_EXTREMES / TIDES_EXTREME_WIDTH / TIDES_EXTREME_TOP_OFFSET
    + HIGH_TIDES_CHART_VERTICAL_OFFSET, TIDES_FULL_HOUR_STEP, LABEL_TICK_HEIGHT

## Colour
Not banded — a continuous interpolated ramp:
  createColorSpectrum, getColorFromSpectrum, getCurrentWeatherColorSpectrum,
  colorSpectrumToColorMapOverlay  (same ramp feeds the map overlay)
  BEAUFORT_UPPER_BOUNDS_METERS_PER_SECOND  (wind stops sit on Beaufort)

## One shared cursor
  SET_CHART_TIMESTAMP, calculatedCurrentTime, getWeatherRouteTideDataAtChartTimestamp,
  getCursorLabelOffset — scrubbing the sheet moves every lane AND the map overlay.

## Tides
  tideStationsV2.json (1.7 MB GeoJSON, ~UKHO/NOAA stations), showTideStationPointer,
  WEATHER_TIDE_STATIONS_DEFAULT_RANGE, TIDE_STATION_NOT_FOUND, TIDE_NO_DATA,
  getPreferredWaveOrTideHeightValue

## What we replicate
Sheet + shared time axis + lanes + continuous spectrum + one cursor driving the map.
Tide source differs: no station harmonics, so Open-Meteo Marine sea_level_height_msl.
