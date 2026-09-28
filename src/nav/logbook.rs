//! The logbook: what the boat actually did, kept small.
//!
//! Every 10 s under way (every 60 s when stopped) the recorder takes one
//! *sample* — position, speeds, courses, depth, wind, pressure, sea
//! temperature, whichever the boat sends fresh — and keeps it in one file per
//! UTC day, `YYYY-MM-DD.nclog`, beside a `YYYY-MM-DD.notes` file of the
//! sailor's own notes.
//!
//! ## The format
//!
//! Each value is rounded to what matters on a boat (position to 1e-6°, about
//! 0.1 m; speeds to 0.1 kn; angles to 1°; depth to 0.1 m) and stored as the
//! *change* from the same value in the sample before, zigzag-encoded as a
//! LEB128 varint: a boat sailing steadily costs one byte per value. A
//! presence mask leads each sample, so a sensor the boat lacks costs
//! nothing. About 10–20 bytes a sample: 17 with all thirteen values present
//! and changing, about 7 KB an hour under way with the block framing.
//!
//! Samples are written in *blocks* — a magic number, the length, a CRC-32,
//! then the samples, the first of them absolute — appended every couple of
//! minutes and synced. A block cut short by a power failure fails its CRC
//! and is skipped; the reader finds the next magic and carries on, so a
//! crash costs the unfinished block, never the rest of the day. No database
//! and no compression library: nothing is ever asked of the data but "add a
//! sample" and "read a day", and at ~7 KB an hour a season fits in a few
//! megabytes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};

use crate::geo::LatLon;

/// What a sample holds, in order of the presence mask's bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Lat,
    Lon,
    /// Speed over ground, knots.
    Sog,
    /// Course over ground, degrees true.
    Cog,
    /// Heading, degrees true.
    Heading,
    /// Speed through water, knots.
    Stw,
    /// Depth under the boat, metres.
    Depth,
    /// Apparent wind speed, knots.
    Aws,
    /// Apparent wind angle, degrees, − port / + starboard.
    Awa,
    /// True wind speed, knots.
    Tws,
    /// True wind direction (from), degrees true.
    Twd,
    /// Pressure, hPa.
    Baro,
    /// Sea temperature, °C.
    WaterTemp,
}

pub const FIELDS: [Field; 13] = [
    Field::Lat,
    Field::Lon,
    Field::Sog,
    Field::Cog,
    Field::Heading,
    Field::Stw,
    Field::Depth,
    Field::Aws,
    Field::Awa,
    Field::Tws,
    Field::Twd,
    Field::Baro,
    Field::WaterTemp,
];

#[derive(Clone, Copy)]
enum Wrap {
    No,
    /// 0..360
    Full,
    /// −180..180
    Half,
}

impl Field {
    /// Units per stored integer step, and how the value wraps.
    fn scale(self) -> (f64, Wrap) {
        match self {
            Field::Lat | Field::Lon => (1e6, Wrap::No),
            Field::Cog | Field::Heading | Field::Twd => (1.0, Wrap::Full),
            Field::Awa => (1.0, Wrap::Half),
            _ => (10.0, Wrap::No),
        }
    }

    /// A column name, for CSV.
    pub fn name(self) -> &'static str {
        match self {
            Field::Lat => "lat",
            Field::Lon => "lon",
            Field::Sog => "sog_kn",
            Field::Cog => "cog_deg",
            Field::Heading => "heading_deg",
            Field::Stw => "stw_kn",
            Field::Depth => "depth_m",
            Field::Aws => "aws_kn",
            Field::Awa => "awa_deg",
            Field::Tws => "tws_kn",
            Field::Twd => "twd_deg",
            Field::Baro => "pressure_hpa",
            Field::WaterTemp => "water_c",
        }
    }
}

/// One moment of the boat, quantized.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sample {
    /// Unix seconds, UTC.
    pub t: i64,
    q: [Option<i64>; 13],
}

impl Sample {
    pub fn new(t: i64) -> Self {
        Self { t, q: [None; 13] }
    }

    /// Set a value in its natural unit (see [`Field`]); non-finite is none.
    pub fn set(&mut self, f: Field, v: Option<f64>) {
        let (scale, wrap) = f.scale();
        self.q[f as usize] = v.filter(|v| v.is_finite()).map(|v| {
            let q = (v * scale).round() as i64;
            match wrap {
                Wrap::No => q,
                Wrap::Full => q.rem_euclid(360),
                Wrap::Half => (q + 180).rem_euclid(360) - 180,
            }
        });
    }

    pub fn get(&self, f: Field) -> Option<f64> {
        self.q[f as usize].map(|q| q as f64 / f.scale().0)
    }

    pub fn position(&self) -> Option<LatLon> {
        Some(LatLon::new(self.get(Field::Lat)?, self.get(Field::Lon)?))
    }

    pub fn time(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.t, 0).unwrap_or_default()
    }

    fn mask(&self) -> u64 {
        self.q.iter().enumerate().fold(0, |m, (i, v)| if v.is_some() { m | 1 << i } else { m })
    }
}

// ---- varints, zigzag, CRC-32 -------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn get_varint(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *data.get(*at)?;
        *at += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// CRC-32 (IEEE), bitwise: a block is a few hundred bytes a minute, and a
/// table would be the only thing here worth optimizing.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

const FILE_MAGIC: &[u8] = b"NCLOG1\n";
const BLOCK_MAGIC: [u8; 4] = [0xA7, b'L', b'G', b'B'];

/// The change from `prev` to `v`, the short way round for an angle.
fn delta(f: Field, prev: i64, v: i64) -> i64 {
    match f.scale().1 {
        Wrap::No => v - prev,
        Wrap::Full | Wrap::Half => (v - prev + 180).rem_euclid(360) - 180,
    }
}

fn apply(f: Field, prev: i64, d: i64) -> i64 {
    match f.scale().1 {
        Wrap::No => prev + d,
        Wrap::Full => (prev + d).rem_euclid(360),
        Wrap::Half => (prev + d + 180).rem_euclid(360) - 180,
    }
}

/// Encode a run of samples as one block. The first sample is absolute; each
/// later value is a change from the same value in the sample before, or
/// absolute where that sample lacked it.
pub fn encode_block(samples: &[Sample]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(samples.len() * 14);
    let mut prev: Option<&Sample> = None;
    for s in samples {
        put_varint(&mut payload, s.mask());
        match prev {
            None => put_varint(&mut payload, zigzag(s.t)),
            Some(p) => put_varint(&mut payload, zigzag(s.t - p.t)),
        }
        for f in FIELDS {
            let Some(v) = s.q[f as usize] else { continue };
            let code = match prev.and_then(|p| p.q[f as usize]) {
                Some(pv) => zigzag(delta(f, pv, v)),
                None => zigzag(v),
            };
            put_varint(&mut payload, code);
        }
        prev = Some(s);
    }
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(&BLOCK_MAGIC);
    put_varint(&mut out, payload.len() as u64);
    out.extend_from_slice(&crc32(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn decode_payload(payload: &[u8], out: &mut Vec<Sample>) -> Option<()> {
    let mut at = 0;
    let mut prev: Option<Sample> = None;
    let mut decoded = Vec::new();
    while at < payload.len() {
        let mask = get_varint(payload, &mut at)?;
        let t = unzigzag(get_varint(payload, &mut at)?);
        let mut s = Sample::new(prev.map_or(t, |p| p.t + t));
        for f in FIELDS {
            if mask & (1 << f as usize) == 0 {
                continue;
            }
            let code = unzigzag(get_varint(payload, &mut at)?);
            s.q[f as usize] = Some(match prev.and_then(|p| p.q[f as usize]) {
                Some(pv) => apply(f, pv, code),
                None => code,
            });
        }
        decoded.push(s);
        prev = Some(s);
    }
    out.extend(decoded);
    Some(())
}

/// Every sample in a day file's bytes. A damaged block is skipped and the
/// reader resynchronises on the next block's magic.
pub fn decode(data: &[u8]) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut at = if data.starts_with(FILE_MAGIC) { FILE_MAGIC.len() } else { 0 };
    while at + BLOCK_MAGIC.len() <= data.len() {
        if data[at..at + 4] != BLOCK_MAGIC {
            at += 1;
            continue;
        }
        let mut p = at + 4;
        let ok = (|| {
            let len = get_varint(data, &mut p)? as usize;
            let crc = u32::from_le_bytes(data.get(p..p + 4)?.try_into().ok()?);
            let payload = data.get(p + 4..p + 4 + len)?;
            if crc32(payload) != crc {
                return None;
            }
            decode_payload(payload, &mut out)?;
            Some(p + 4 + len)
        })();
        match ok {
            Some(next) => at = next,
            None => at += 1,
        }
    }
    out
}

// ---- files -----------------------------------------------------------------

/// Where the logbook lives.
pub fn default_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("navcore").join("logbook"))
}

/// Tests, captures and the like point this elsewhere: `NAVCORE_LOGBOOK`.
pub fn dir() -> PathBuf {
    std::env::var("NAVCORE_LOGBOOK")
        .map(PathBuf::from)
        .ok()
        .or_else(default_dir)
        .unwrap_or_else(|| PathBuf::from("logbook"))
}

pub fn day_of(t: i64) -> NaiveDate {
    DateTime::from_timestamp(t, 0).unwrap_or_default().date_naive()
}

pub fn log_path(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{day}.nclog"))
}

fn notes_path(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{day}.notes"))
}

/// Every day with a log or notes, newest first.
pub fn days(dir: &Path) -> Vec<NaiveDate> {
    let mut out: Vec<NaiveDate> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".nclog").or_else(|| name.strip_suffix(".notes"))?;
            NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()
        })
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.dedup();
    out
}

pub fn read_day(dir: &Path, day: NaiveDate) -> Vec<Sample> {
    std::fs::read(log_path(dir, day)).map(|d| decode(&d)).unwrap_or_default()
}

/// Bytes on disk for a day, log and notes.
pub fn day_bytes(dir: &Path, day: NaiveDate) -> u64 {
    [log_path(dir, day), notes_path(dir, day)]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

pub fn delete_day(dir: &Path, day: NaiveDate) {
    let _ = std::fs::remove_file(log_path(dir, day));
    let _ = std::fs::remove_file(notes_path(dir, day));
}

// ---- notes -----------------------------------------------------------------

/// A line in the logbook in the sailor's own words.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    /// Unix seconds, UTC.
    pub t: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    pub text: String,
}

impl Note {
    pub fn position(&self) -> Option<LatLon> {
        Some(LatLon::new(self.lat?, self.lon?))
    }
}

/// A day's notes, in time order. One JSON object a line, so a note is an
/// append and a damaged line loses only itself.
pub fn read_notes(dir: &Path, day: NaiveDate) -> Vec<Note> {
    let text = std::fs::read_to_string(notes_path(dir, day)).unwrap_or_default();
    let mut notes: Vec<Note> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    notes.sort_by_key(|n| n.t);
    notes
}

pub fn add_note(dir: &Path, note: &Note) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let line = serde_json::to_string(note).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes_path(dir, day_of(note.t)))?;
    writeln!(f, "{line}")?;
    f.sync_data()
}

/// Remove a note (by its exact content), rewriting the day's file.
pub fn delete_note(dir: &Path, note: &Note) -> std::io::Result<()> {
    let day = day_of(note.t);
    let kept: Vec<String> = read_notes(dir, day)
        .into_iter()
        .filter(|n| n != note)
        .filter_map(|n| serde_json::to_string(&n).ok())
        .collect();
    let path = notes_path(dir, day);
    if kept.is_empty() {
        return std::fs::remove_file(path).or(Ok(()));
    }
    let tmp = path.with_extension("notes.tmp");
    std::fs::write(&tmp, kept.join("\n") + "\n")?;
    std::fs::rename(tmp, path)
}

// ---- summary ---------------------------------------------------------------

/// Under this over ground the boat is stopped.
pub const MOVING_KN: f64 = 0.5;

/// A day at a glance.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub samples: usize,
    pub first: Option<Sample>,
    pub last: Option<Sample>,
    pub distance_nm: f64,
    pub underway_s: i64,
    pub max_sog: Option<f64>,
    pub max_tws: Option<f64>,
    pub max_aws: Option<f64>,
    pub min_depth: Option<f64>,
}

impl Summary {
    /// Average over ground while under way.
    pub fn avg_sog(&self) -> Option<f64> {
        (self.underway_s > 60).then(|| self.distance_nm / (self.underway_s as f64 / 3600.0))
    }

    pub fn start(&self) -> Option<LatLon> {
        self.first.and_then(|s| s.position())
    }

    pub fn end(&self) -> Option<LatLon> {
        self.last.and_then(|s| s.position())
    }
}

pub fn summarize(samples: &[Sample]) -> Summary {
    let max = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let mut s = Summary {
        samples: samples.len(),
        first: samples.first().copied(),
        last: samples.last().copied(),
        ..Default::default()
    };
    let mut last_pos: Option<(LatLon, i64)> = None;
    for (i, x) in samples.iter().enumerate() {
        s.max_sog = max(s.max_sog, x.get(Field::Sog));
        s.max_tws = max(s.max_tws, x.get(Field::Tws));
        s.max_aws = max(s.max_aws, x.get(Field::Aws));
        s.min_depth = match (s.min_depth, x.get(Field::Depth)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(p) = x.position() {
            if let Some((q, _)) = last_pos {
                s.distance_nm += crate::geo::distance_m(q, p) / crate::geo::METRES_PER_NM;
            }
            last_pos = Some((p, x.t));
        }
        // Time under way: a sample moving, and not after a gap in the log.
        if i > 0 {
            let dt = x.t - samples[i - 1].t;
            if dt <= 180 && x.get(Field::Sog).is_some_and(|v| v >= MOVING_KN) {
                s.underway_s += dt;
            }
        }
    }
    s
}

// ---- export ----------------------------------------------------------------

/// The day's positions as a GPX 1.1 track, for any other plotter.
pub fn to_gpx(day: NaiveDate, samples: &[Sample], notes: &[Note]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <gpx version=\"1.1\" creator=\"navcore\" xmlns=\"http://www.topografix.com/GPX/1/1\">\n",
    );
    let esc = |t: &str| quick_xml::escape::escape(t).into_owned();
    for n in notes {
        if let Some(p) = n.position() {
            s.push_str(&format!(
                "  <wpt lat=\"{}\" lon=\"{}\"><time>{}</time><name>{}</name></wpt>\n",
                p.lat,
                p.lon,
                DateTime::from_timestamp(n.t, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ"),
                esc(&n.text)
            ));
        }
    }
    s.push_str(&format!("  <trk>\n    <name>Log {day}</name>\n    <trkseg>\n"));
    for x in samples {
        if let Some(p) = x.position() {
            s.push_str(&format!(
                "      <trkpt lat=\"{}\" lon=\"{}\"><time>{}</time></trkpt>\n",
                p.lat,
                p.lon,
                x.time().format("%Y-%m-%dT%H:%M:%SZ")
            ));
        }
    }
    s.push_str("    </trkseg>\n  </trk>\n</gpx>\n");
    s
}

/// Every sample, one row each, blank where a value was not recorded.
pub fn to_csv(samples: &[Sample]) -> String {
    let mut s = String::from("time_utc");
    for f in FIELDS {
        s.push(',');
        s.push_str(f.name());
    }
    s.push('\n');
    for x in samples {
        s.push_str(&x.time().format("%Y-%m-%dT%H:%M:%SZ").to_string());
        for f in FIELDS {
            s.push(',');
            if let Some(v) = x.get(f) {
                s.push_str(&v.to_string());
            }
        }
        s.push('\n');
    }
    s
}

/// Where exports go: the Downloads folder where there is one, else beside
/// the logbook.
pub fn export_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| dir().join("export"))
}

// ---- the recorder ----------------------------------------------------------

/// A sample every this often under way…
pub const EVERY_MOVING: Duration = Duration::from_secs(10);
/// …and this often stopped.
pub const EVERY_STOPPED: Duration = Duration::from_secs(60);
/// Samples are written in a block this often under way…
const FLUSH_MOVING: Duration = Duration::from_secs(120);
/// …and this often stopped: a block's first sample is absolute, so a block
/// a minute at anchor would be mostly header.
const FLUSH_STOPPED: Duration = Duration::from_secs(600);

/// Takes samples at the right pace and appends them to the day's file.
#[derive(Debug)]
pub struct Recorder {
    dir: PathBuf,
    /// Today's samples, written and not, for the chart and the summary.
    pub today: Vec<Sample>,
    pub day: Option<NaiveDate>,
    /// How many of `today` are not yet written.
    pending: usize,
    last_sample: Option<Instant>,
    last_flush: Option<Instant>,
    moving: bool,
}

impl Recorder {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            today: Vec::new(),
            day: None,
            pending: 0,
            last_sample: None,
            last_flush: None,
            moving: false,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Offer a sample; it is kept if it is time for one. Returns whether it
    /// was. Writes a block when one is due.
    pub fn offer(&mut self, s: Sample, now: Instant) -> bool {
        let day = day_of(s.t);
        if self.day != Some(day) {
            // Midnight UTC, or the first sample since starting: finish the
            // old day's file and pick up the new one where it left off.
            self.flush();
            self.today = read_day(&self.dir, day);
            self.day = Some(day);
        }
        let moving = s.get(Field::Sog).is_some_and(|v| v >= MOVING_KN);
        let every = if moving { EVERY_MOVING } else { EVERY_STOPPED };
        // Starting to move takes a sample at once: the departure matters.
        let due = self.last_sample.is_none_or(|t| now.duration_since(t) >= every)
            || (moving && !self.moving);
        self.moving = moving;
        if !due || self.today.last().is_some_and(|l| l.t >= s.t) {
            return false;
        }
        self.today.push(s);
        self.pending += 1;
        self.last_sample = Some(now);
        let every_flush = if moving { FLUSH_MOVING } else { FLUSH_STOPPED };
        if self.last_flush.is_none_or(|t| now.duration_since(t) >= every_flush) {
            self.flush();
            self.last_flush = Some(now);
        }
        true
    }

    /// Append what is not yet written, as one block, and sync it.
    pub fn flush(&mut self) {
        let (Some(day), true) = (self.day, self.pending > 0) else { return };
        let from = self.today.len() - self.pending;
        let block = encode_block(&self.today[from..]);
        match append(&log_path(&self.dir, day), &block) {
            Ok(()) => self.pending = 0,
            Err(e) => log::warn!("logbook: could not write {}: {e}", self.dir.display()),
        }
    }
}

fn append(path: &Path, block: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    if f.metadata()?.len() == 0 {
        f.write_all(FILE_MAGIC)?;
    }
    f.write_all(block)?;
    f.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: i64, lat: f64, lon: f64, sog: f64, cog: f64) -> Sample {
        let mut s = Sample::new(t);
        s.set(Field::Lat, Some(lat));
        s.set(Field::Lon, Some(lon));
        s.set(Field::Sog, Some(sog));
        s.set(Field::Cog, Some(cog));
        s
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("navcore-logbook-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_block_round_trips_exactly() {
        let mut a = sample(1_790_000_000, 57.058512, 9.894987, 5.4, 359.0);
        a.set(Field::Awa, Some(-35.0));
        a.set(Field::Depth, Some(4.2));
        let mut b = sample(1_790_000_010, 57.058712, 9.895187, 5.6, 2.0); // across north
        b.set(Field::Awa, Some(178.0)); // across dead aft, the long way
        let c = sample(1_790_000_020, 57.058912, 9.895387, 5.5, 3.0); // no depth, no wind
        let back = decode(&encode_block(&[a, b, c]));
        assert_eq!(back, vec![a, b, c]);
        assert_eq!(back[1].get(Field::Cog), Some(2.0));
        assert_eq!(back[1].get(Field::Awa), Some(178.0));
    }

    #[test]
    fn a_steady_sail_costs_a_dozen_bytes_a_sample() {
        let mut samples = Vec::new();
        for i in 0..360 {
            let mut s = sample(1_790_000_000 + i * 10, 57.0 + i as f64 * 1.5e-4, 10.0, 5.4, 12.0);
            for (f, v) in [(Field::Heading, 15.0), (Field::Stw, 5.1), (Field::Depth, 12.0 + (i % 7) as f64 * 0.1),
                           (Field::Aws, 14.0), (Field::Awa, 48.0), (Field::Tws, 11.0), (Field::Twd, 240.0)] {
                s.set(f, Some(v));
            }
            samples.push(s);
        }
        let bytes = encode_block(&samples).len();
        let per = bytes as f64 / samples.len() as f64;
        assert!(per < 16.0, "{per:.1} bytes a sample");
        assert_eq!(decode(&encode_block(&samples)), samples);
    }

    #[test]
    fn a_torn_block_loses_only_itself() {
        let a = encode_block(&[sample(100, 55.0, 11.0, 0.0, 0.0)]);
        let b = encode_block(&[sample(200, 55.1, 11.1, 0.0, 0.0)]);
        let c = encode_block(&[sample(300, 55.2, 11.2, 0.0, 0.0)]);
        let mut file = FILE_MAGIC.to_vec();
        file.extend_from_slice(&a);
        file.extend_from_slice(&b[..b.len() - 3]); // power cut mid-write
        file.extend_from_slice(&c);
        let back = decode(&file);
        assert_eq!(back.iter().map(|s| s.t).collect::<Vec<_>>(), vec![100, 300]);
    }

    #[test]
    fn the_recorder_paces_itself_and_the_file_reads_back() {
        let dir = scratch("rec");
        let mut r = Recorder::new(dir.clone());
        let t0 = Instant::now();
        let base = 1_790_000_000;
        let mut kept = 0;
        for i in 0..120 {
            // Moving for 60 s-steps of 1 s: one sample per 10 s.
            if r.offer(sample(base + i, 57.0 + i as f64 * 1e-5, 10.0, 5.0, 90.0), t0 + Duration::from_secs(i as u64)) {
                kept += 1;
            }
        }
        assert_eq!(kept, 12);
        r.flush();
        let back = read_day(&dir, day_of(base));
        assert_eq!(back.len(), 12);
        assert_eq!(back, r.today);
        let sum = summarize(&back);
        assert!(sum.underway_s >= 100, "{}", sum.underway_s);
        assert!(sum.distance_nm > 0.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notes_add_read_and_delete() {
        let dir = scratch("notes");
        let n1 = Note { t: 1_790_000_000, lat: Some(57.0), lon: Some(10.0), text: "Reefed, wind 22 kn".into() };
        let n2 = Note { t: 1_790_000_600, lat: None, lon: None, text: "Tea".into() };
        add_note(&dir, &n2).unwrap();
        add_note(&dir, &n1).unwrap();
        let day = day_of(n1.t);
        assert_eq!(read_notes(&dir, day), vec![n1.clone(), n2.clone()]);
        assert_eq!(days(&dir), vec![day]);
        delete_note(&dir, &n1).unwrap();
        assert_eq!(read_notes(&dir, day), vec![n2]);
        let gpx = to_gpx(day, &[sample(n1.t, 57.0, 10.0, 1.0, 0.0)], &[n1]);
        assert!(gpx.contains("<name>Reefed, wind 22 kn</name>"));
        assert!(crate::nav::gpx::parse(&gpx).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn csv_has_a_column_per_field_and_blanks_for_missing() {
        let csv = to_csv(&[sample(1_790_000_000, 57.0, 10.0, 5.0, 90.0)]);
        let mut lines = csv.lines();
        assert!(lines.next().unwrap().starts_with("time_utc,lat,lon,sog_kn"));
        let row = lines.next().unwrap();
        assert_eq!(row.split(',').count(), 14);
        assert!(row.contains(",57,10,5,90,"));
    }
}
