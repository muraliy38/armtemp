//! Core Temp-style temperature logging to CSV.
//!
//! While logging is on, one row of real telemetry is appended per logging
//! interval to a timestamped `.csv` in the app's log folder. Readings with no
//! live source this tick are written as empty cells — never a fabricated
//! value. Each row goes straight to the OS (no userspace buffer), so a crash
//! loses at most the row in flight. Files are UTF-8 with a BOM so Excel shows
//! the `°` in the headers correctly.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::sensors::types::SensorSnapshot;

pub const DEFAULT_INTERVAL_SECS: u64 = 10;
pub const MIN_INTERVAL_SECS: u64 = 1;
pub const MAX_INTERVAL_SECS: u64 = 3600;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// What the frontend (status bar, menus, Settings) is told about logging.
#[derive(Clone, Debug, Serialize)]
pub struct LogStatus {
    pub active: bool,
    /// Folder the CSV files are written to.
    pub dir: String,
    /// File currently being appended to; `None` until the first row lands.
    pub file: Option<String>,
    /// Rows written to the current file.
    pub rows: u64,
    /// Why logging stopped (couldn't create the folder/file, a write failed).
    pub error: Option<String>,
}

/// A file's columns are fixed for its lifetime: switching °C/°F or a change
/// in the reported core count starts a fresh file so the header always
/// matches the rows beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    unit_f: bool,
    cores: usize,
}

struct OpenLog {
    file: File,
    path: PathBuf,
    layout: Layout,
}

pub struct TempLogger {
    dir: PathBuf,
    active: bool,
    open: Option<OpenLog>,
    next_due: Option<Instant>,
    rows: u64,
    error: Option<String>,
}

impl TempLogger {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, active: false, open: None, next_due: None, rows: 0, error: None }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn current_file(&self) -> Option<&Path> {
        self.open.as_ref().map(|o| o.path.as_path())
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn status(&self) -> LogStatus {
        LogStatus {
            active: self.active,
            dir: self.dir.display().to_string(),
            file: self.open.as_ref().map(|o| o.path.display().to_string()),
            rows: self.rows,
            error: self.error.clone(),
        }
    }

    /// Turn logging on. The folder is created right away so a permissions
    /// problem surfaces immediately rather than on the first tick; the file
    /// itself is created with the first row (its header needs a snapshot).
    pub fn start(&mut self) -> Result<(), String> {
        if self.active {
            return Ok(());
        }
        if let Err(e) = fs::create_dir_all(&self.dir) {
            let msg = format!("Couldn't create log folder {}: {e}", self.dir.display());
            self.error = Some(msg.clone());
            return Err(msg);
        }
        self.active = true;
        self.open = None;
        self.next_due = None;
        self.rows = 0;
        self.error = None;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.active = false;
        self.open = None;
        self.next_due = None;
    }

    /// Feed a fresh snapshot. Writes a row when logging is on and the
    /// interval has elapsed. Returns true when the status changed in a way
    /// the UI should hear about (a new file was opened, or logging stopped
    /// on an error) — row counts alone don't warrant an event every tick.
    pub fn on_snapshot(&mut self, snap: &SensorSnapshot, unit_f: bool, interval: Duration) -> bool {
        if !self.active {
            return false;
        }
        let now = Instant::now();
        if matches!(self.next_due, Some(due) if now < due) {
            return false;
        }
        // Rows land on poll ticks, so schedule from the previous due time
        // (not `now`) to keep the long-run spacing at the chosen interval
        // instead of drifting late by up to one poll period per row.
        self.next_due = Some(match self.next_due {
            Some(due) if now.duration_since(due) < interval => due + interval,
            _ => now + interval,
        });

        let layout = Layout { unit_f, cores: snap.cores.len() };
        let mut opened = false;
        if self.open.as_ref().map(|o| o.layout) != Some(layout) {
            match create_log_file(&self.dir, layout, &local_now()) {
                Ok(open) => {
                    self.open = Some(open);
                    self.rows = 0;
                    opened = true;
                }
                Err(e) => {
                    self.fail(format!("Couldn't create log file in {}: {e}", self.dir.display()));
                    return true;
                }
            }
        }

        let line = format_row(snap, unit_f, &local_now());
        let open = self.open.as_mut().expect("log file opened above");
        if let Err(e) = open.file.write_all(line.as_bytes()) {
            let msg = format!("Writing {} failed: {e}", open.path.display());
            self.fail(msg);
            return true;
        }
        self.rows += 1;
        opened
    }

    fn fail(&mut self, msg: String) {
        eprintln!("[armtemp] logging stopped: {msg}");
        self.error = Some(msg);
        self.stop();
    }
}

fn create_log_file(dir: &Path, layout: Layout, now: &Stamp) -> std::io::Result<OpenLog> {
    fs::create_dir_all(dir)?;
    let base = format!("ARMtemp Log {}", now.file_stamp());
    // Two files in the same second (e.g. a quick °C/°F flip) get a suffix
    // rather than clobbering each other.
    for n in 1..100 {
        let name = if n == 1 { format!("{base}.csv") } else { format!("{base} ({n}).csv") };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(UTF8_BOM)?;
                file.write_all(format_header(layout.unit_f, layout.cores).as_bytes())?;
                return Ok(OpenLog { file, path, layout });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(ErrorKind::AlreadyExists, "too many log files with the same timestamp"))
}

/// Column header row. Temperature is the app's single honest CPU temperature
/// (hottest valid ACPI zone) — there is no per-core temperature on this
/// firmware, so per-core columns are load only.
pub fn format_header(unit_f: bool, cores: usize) -> String {
    let u = if unit_f { "F" } else { "C" };
    let mut cols = vec![
        "Time".to_string(),
        format!("CPU Temp (°{u})"),
        "Throttling".to_string(),
        "CPU Speed (MHz)".to_string(),
        "Avg Load (%)".to_string(),
    ];
    cols.extend((0..cores).map(|i| format!("Core #{i} Load (%)")));
    let mut line = cols.join(",");
    line.push_str("\r\n");
    line
}

pub fn format_row(snap: &SensorSnapshot, unit_f: bool, now: &Stamp) -> String {
    let temp = snap
        .package_c
        .map(|c| if unit_f { c * 9.0 / 5.0 + 32.0 } else { c })
        .map(|t| format!("{t:.1}"))
        .unwrap_or_default();
    // No valid zones this tick means throttle state is unknown, not "No".
    let throttling = if snap.zones.is_empty() {
        String::new()
    } else if snap.zones.iter().any(|z| z.throttled) {
        "Yes".to_string()
    } else {
        "No".to_string()
    };
    let speed = snap.clock_mhz.map(|m| m.to_string()).unwrap_or_default();
    let loads: Vec<f64> = snap.cores.iter().filter_map(|c| c.load).collect();
    let avg = if loads.is_empty() {
        String::new()
    } else {
        format!("{:.0}", loads.iter().sum::<f64>() / loads.len() as f64)
    };

    let mut cols = vec![now.row_stamp(), temp, throttling, speed, avg];
    cols.extend(snap.cores.iter().map(|c| c.load.map(|l| format!("{l:.0}")).unwrap_or_default()));
    let mut line = cols.join(",");
    line.push_str("\r\n");
    line
}

/// Wall-clock local time, split out so formatting is testable.
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

impl Stamp {
    /// `2026-10-19 14:30:05` — sorts correctly and Excel parses it as a date.
    fn row_stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-10-19 14-30-05` — the same, minus characters Windows forbids in
    /// file names.
    fn file_stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(windows)]
fn local_now() -> Stamp {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    Stamp {
        year: t.wYear,
        month: t.wMonth,
        day: t.wDay,
        hour: t.wHour,
        minute: t.wMinute,
        second: t.wSecond,
    }
}

/// Non-Windows builds (tests, `cargo check` on a dev box) log in UTC; the
/// shipped Windows binary always uses local time above.
#[cfg(not(windows))]
fn local_now() -> Stamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Stamp {
        year: year as u16,
        month: month as u16,
        day: day as u16,
        hour: (rem / 3600) as u16,
        minute: (rem % 3600 / 60) as u16,
        second: (rem % 60) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::{CoreKind, CoreReading, ZoneReading};

    fn core(index: u32, load: Option<f64>) -> CoreReading {
        CoreReading {
            index,
            kind: CoreKind::Performance,
            kind_label: "P".into(),
            kind_title: "Performance core".into(),
            load,
            load_min: None,
            load_max: None,
            load_avg: None,
        }
    }

    fn snap() -> SensorSnapshot {
        SensorSnapshot {
            package_c: Some(52.26),
            clock_mhz: Some(3417),
            zones: vec![ZoneReading { name: r"\_SB.TZ0".into(), temp_c: 52.26, throttled: false }],
            cores: vec![core(0, Some(10.4)), core(1, Some(15.6)), core(2, None)],
            ..SensorSnapshot::default()
        }
    }

    const NOW: Stamp = Stamp { year: 2026, month: 10, day: 19, hour: 9, minute: 5, second: 3 };

    #[test]
    fn header_matches_core_count_and_unit() {
        assert_eq!(
            format_header(false, 2),
            "Time,CPU Temp (°C),Throttling,CPU Speed (MHz),Avg Load (%),Core #0 Load (%),Core #1 Load (%)\r\n"
        );
        assert!(format_header(true, 0).contains("CPU Temp (°F)"));
    }

    #[test]
    fn row_formats_real_values_and_leaves_missing_ones_empty() {
        assert_eq!(format_row(&snap(), false, &NOW), "2026-10-19 09:05:03,52.3,No,3417,13,10,16,\r\n");
        assert_eq!(format_row(&snap(), true, &NOW), "2026-10-19 09:05:03,126.1,No,3417,13,10,16,\r\n");

        let mut s = snap();
        s.package_c = None;
        s.clock_mhz = None;
        s.zones.clear();
        assert_eq!(format_row(&s, false, &NOW), "2026-10-19 09:05:03,,,,13,10,16,\r\n");

        s.zones.push(ZoneReading { name: "z".into(), temp_c: 99.0, throttled: true });
        assert!(format_row(&s, false, &NOW).contains(",Yes,"));
    }

    #[test]
    fn logger_writes_header_then_rows_and_rotates_on_unit_change() {
        let dir = std::env::temp_dir().join(format!("armtemp-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut log = TempLogger::new(dir.clone());
        log.start().unwrap();

        assert!(log.on_snapshot(&snap(), false, Duration::ZERO), "first row opens a file");
        assert!(!log.on_snapshot(&snap(), false, Duration::ZERO));
        let first = log.current_file().unwrap().to_path_buf();
        let text = fs::read(&first).unwrap();
        assert!(text.starts_with(UTF8_BOM));
        let text = String::from_utf8(text[3..].to_vec()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.starts_with("Time,CPU Temp (°C)"));

        // A row is due now; after it, the hour-long interval holds off the next.
        assert!(!log.on_snapshot(&snap(), false, Duration::from_secs(3600)));
        assert!(!log.on_snapshot(&snap(), false, Duration::from_secs(3600)));
        assert_eq!(log.status().rows, 3);

        // Unit flip starts a new file (same second -> suffixed name).
        log.next_due = None;
        assert!(log.on_snapshot(&snap(), true, Duration::ZERO));
        let second = log.current_file().unwrap().to_path_buf();
        assert_ne!(first, second);
        assert!(fs::read_to_string(&second).unwrap().contains("CPU Temp (°F)"));

        log.stop();
        assert!(!log.on_snapshot(&snap(), false, Duration::ZERO));
        assert!(!log.status().active);
        let _ = fs::remove_dir_all(&dir);
    }
}
