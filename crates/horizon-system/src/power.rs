//! The machine's power state as the OS describes it, sampled by one thread at 1 Hz and read as a
//! snapshot ([`sample`], [`transitions`]) by the rest of the game.
//!
//! The frame loop never asks the OS: [`start`] spawns one thread named `horizon-power` that reads
//! it once a second and publishes what it found into atomics, and every reader takes those. What
//! a running game cannot see for itself lives here: what powers the machine (a laptop's cable),
//! whether the OS is saving power, and whether it limits the CPU to stay cool. A fourth count
//! says how often the machine slept: no tick of a 1 Hz thread lasts [`SLEEP_GAP`], so a larger gap
//! between two of them is a suspend the game slept through (#18240).
//!
//! Nothing here decides anything. Horizon's Battery Mode stays the player's own setting
//! (`horizon_client_render::graphics::Graphics::battery`); this channel only says what the OS is
//! doing, so the game can tell the player instead of leaving them to guess.

use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// How often the sampler reads the OS.
const TICK: Duration = Duration::from_secs(1);

/// How far apart two ticks must be for a sleep to explain them and not a busy machine: no stall
/// of a thread that runs once a second lasts this long, and a closed lid lasts minutes.
const SLEEP_GAP: Duration = Duration::from_secs(30);

/// Where the kernel lists the machine's power supplies.
#[cfg(all(unix, not(target_os = "macos")))]
const SYSFS: &str = "/sys/class/power_supply";

/// What powers the machine, as the OS names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// Mains power.
    Ac,
    /// The battery: the cable is out or the supply failed.
    Battery,
    /// The OS doesn't say: a desktop whose firmware reports nothing, or a platform this build
    /// has no reader for.
    #[default]
    Unknown,
}

impl Source {
    /// `ac`, `battery`, or empty where the OS doesn't say: the strings the crash report and
    /// telemetry already carry (`crate::Machine::power`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ac => "ac",
            Self::Battery => "battery",
            Self::Unknown => "",
        }
    }

    /// The snapshot's form.
    fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Ac => 1,
            Self::Battery => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Ac,
            2 => Self::Battery,
            _ => Self::Unknown,
        }
    }
}

/// What the OS says about the CPU's speed against its heat, where it says anything at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Thermal {
    /// No thermal report: Windows and Linux have none a game can read (the sysfs files that
    /// would say are root's, and guessing from a temperature is not a report).
    #[default]
    Unknown,
    /// Reported, and the CPU runs at full speed.
    Nominal,
    /// Reported below full speed: the OS slows the CPU down to stay cool.
    Limited(u8),
}

impl Thermal {
    /// The snapshot's form: 0 unknown, 1 nominal, `percent + 2` below full speed.
    fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Nominal => 1,
            Self::Limited(percent) => percent.saturating_add(2),
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Unknown,
            1 => Self::Nominal,
            code => Self::Limited(code - 2),
        }
    }
}

/// One reading of the channel, as [`sample`] gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub source: Source,
    /// The OS's energy saver is on (Windows' `SystemStatusFlag`). Always `false` where the OS has
    /// no such switch a game can read, as on Linux.
    pub saver: bool,
    pub thermal: Thermal,
    /// How often the machine slept since [`start`], as the sampler's clocks saw it.
    pub resumes: u32,
}

impl Sample {
    /// Whether the OS is holding the game back right now: energy saver on, or the CPU limited
    /// below full speed. What the graphics page warns about.
    pub fn limited(self) -> bool {
        self.saver || matches!(self.thermal, Thermal::Limited(_))
    }
}

/// The published snapshot: [`sample`] reads it without a lock and without the OS.
static SOURCE: AtomicU8 = AtomicU8::new(0);
static SAVER: AtomicBool = AtomicBool::new(false);
static THERMAL: AtomicU8 = AtomicU8::new(0);
static RESUMES: AtomicU32 = AtomicU32::new(0);
static TRANSITIONS: AtomicU32 = AtomicU32::new(0);

/// The channel's state before [`start`], or where its thread couldn't be made: nothing known,
/// which is what a reader already handles.
pub fn sample() -> Sample {
    Sample {
        source: Source::from_code(SOURCE.load(Ordering::Relaxed)),
        saver: SAVER.load(Ordering::Relaxed),
        thermal: Thermal::from_code(THERMAL.load(Ordering::Relaxed)),
        resumes: RESUMES.load(Ordering::Relaxed),
    }
}

/// How many changes the channel has announced since [`start`]: a new source, another energy-saver
/// or thermal state, or a sleep. A reader that remembers the number it last acted on and compares
/// it logs one line per change, never per tick, whatever cadence it runs at. A machine whose
/// source the OS never names (a desktop, a platform without a probe) never moves, so it announces
/// nothing.
pub fn transitions() -> u32 {
    TRANSITIONS.load(Ordering::Relaxed)
}

/// Starts the sampler, at most once per process: one thread named `horizon-power`, reading the OS
/// at 1 Hz for the life of the game. A second call does nothing, so the game may start it from
/// wherever it starts its other background work.
pub fn start() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        // No thread is a state to read too (`sample` stays unknown); it is not worth a panic.
        let _ = std::thread::Builder::new()
            .name("horizon-power".into())
            .spawn(run);
    });
}

/// The power source the OS names right now, read once on this thread: what [`crate::power`] (the
/// crash report's and telemetry's `ac`/`battery`) asks. A loop reads [`sample`] instead: this
/// starts a process on macOS.
pub fn read_source() -> Source {
    read_os(&mut OsProbe::default()).source
}

fn run() {
    let mut channel = Channel::default();
    let mut probe = OsProbe::default();
    let mut clock = Clock::now();
    loop {
        let now = Clock::now();
        let slept = clock.slept_to(now);
        clock = now;
        let sample = channel.observe(read_os(&mut probe), slept);
        SOURCE.store(sample.source.code(), Ordering::Relaxed);
        SAVER.store(sample.saver, Ordering::Relaxed);
        THERMAL.store(sample.thermal.code(), Ordering::Relaxed);
        RESUMES.store(sample.resumes, Ordering::Relaxed);
        TRANSITIONS.store(channel.transitions, Ordering::Relaxed);
        std::thread::sleep(TICK);
    }
}

/// One OS reading, before the sampler adds the counts of what it announced: what a parser gives
/// and the state machine compares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Reading {
    source: Source,
    saver: bool,
    thermal: Thermal,
}

/// The sampler's own state: the reading it published last, and the counts it publishes. The
/// thread owns one; a test drives the same state machine without a thread or a clock.
#[derive(Debug, Default)]
struct Channel {
    last: Option<Reading>,
    transitions: u32,
    resumes: u32,
}

impl Channel {
    /// Folds one reading, and whether the machine slept since the last one, in; returns what
    /// [`sample`] gives from then on.
    fn observe(&mut self, reading: Reading, slept: bool) -> Sample {
        if announced(self.last, reading, slept) {
            self.transitions = self.transitions.saturating_add(1);
        }
        if slept {
            self.resumes = self.resumes.saturating_add(1);
        }
        self.last = Some(reading);
        Sample {
            source: reading.source,
            saver: reading.saver,
            thermal: reading.thermal,
            resumes: self.resumes,
        }
    }
}

/// Whether a reader hears about this tick: the machine slept, or what the OS says about it
/// changed while it named a source on both sides. The first reading is where the channel starts,
/// not a change: a game that starts on the battery shouldn't announce it as one.
fn announced(last: Option<Reading>, next: Reading, slept: bool) -> bool {
    if slept {
        return true;
    }
    match last {
        Some(last) => {
            last != next && last.source != Source::Unknown && next.source != Source::Unknown
        }
        None => false,
    }
}

/// Both clocks of one tick. A suspend stops the monotonic clock on Linux and not everywhere, and
/// the wall clock jumps either way, so both are read and either can show the sleep.
#[derive(Clone, Copy, Debug)]
struct Clock {
    monotonic: Instant,
    /// Seconds since 1970, UTC.
    wall: i64,
}

impl Clock {
    fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() as i64),
        }
    }

    /// Whether the machine slept between `self` and `after`: one of the clocks is [`SLEEP_GAP`]
    /// or more ahead of the other reading of itself.
    fn slept_to(self, after: Self) -> bool {
        let monotonic = after
            .monotonic
            .saturating_duration_since(self.monotonic)
            .as_secs();
        // A settable wall clock can jump backwards: the gap is a distance, not a difference.
        monotonic.max(after.wall.abs_diff(self.wall)) >= SLEEP_GAP.as_secs()
    }
}

/// The macOS probe: `pmset` is a process to start, so it is asked at most once per [`ASK`] and
/// its answer is kept for the ticks between.
#[cfg(target_os = "macos")]
const ASK: Duration = Duration::from_secs(30);

#[cfg(target_os = "macos")]
#[derive(Default)]
struct OsProbe {
    asked: Option<Instant>,
    reading: Reading,
}

#[cfg(target_os = "macos")]
impl OsProbe {
    fn reading(&mut self) -> Reading {
        if self.asked.is_some_and(|asked| asked.elapsed() < ASK) {
            return self.reading;
        }
        self.asked = Some(Instant::now());
        self.reading = Reading {
            source: macos_source(
                &crate::command_output("pmset", &["-g", "batt"]).unwrap_or_default(),
            ),
            // Low Power Mode has no field `pmset` prints for a game to read.
            saver: false,
            thermal: macos_thermal(
                &crate::command_output("pmset", &["-g", "therm"]).unwrap_or_default(),
            ),
        };
        self.reading
    }
}

#[cfg(not(target_os = "macos"))]
#[derive(Default)]
struct OsProbe;

/// One reading of the OS, what the sampler's thread asks every tick.
fn read_os(probe: &mut OsProbe) -> Reading {
    #[cfg(windows)]
    {
        let _ = probe;
        windows_reading()
    }
    #[cfg(target_os = "macos")]
    {
        probe.reading()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = probe;
        Reading {
            source: power_supply_source(Path::new(SYSFS)),
            // Neither sysfs nor anything else readable without root says whether the OS is
            // saving power or throttling: unknown, never a guess.
            saver: false,
            thermal: Thermal::Unknown,
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = probe;
        Reading::default()
    }
}

/// Linux: the source from one `/sys/class/power_supply` folder, the kernel's list of the
/// machine's supplies. A mains supply that is online is the source; otherwise a battery is; a
/// machine with neither (a desktop, a VM) names none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn power_supply_source(root: &Path) -> Source {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Source::Unknown;
    };
    let mut battery = false;
    for entry in entries.flatten() {
        let read =
            |name: &str| std::fs::read_to_string(entry.path().join(name)).unwrap_or_default();
        match read("type").trim() {
            "Mains" if read("online").trim() == "1" => return Source::Ac,
            "Battery" => battery = true,
            _ => {}
        }
    }
    if battery {
        Source::Battery
    } else {
        Source::Unknown
    }
}

/// macOS: the first line of `pmset -g batt`, `Now drawing from 'AC Power'` or `'Battery Power'`.
/// A Mac without a battery (a desktop) names AC.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_source(text: &str) -> Source {
    match text
        .lines()
        .find_map(|line| line.strip_prefix("Now drawing from"))
    {
        Some(rest) if rest.contains("'Battery Power'") => Source::Battery,
        Some(_) => Source::Ac,
        None => Source::Unknown,
    }
}

/// macOS: `pmset -g therm`'s `CPU_Speed_Limit`, the percentage of its full speed the OS allows
/// (`2026-11-19 15:00:00 +0300 CPU_Speed_Limit = 62`). A machine with no thermal warning
/// recorded prints a `Note:` line instead, and says nothing.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_thermal(text: &str) -> Thermal {
    let limit = text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        // The line carries a timestamp before the setting's name (`2026-11-19 15:00:00 +0300
        // CPU_Speed_Limit = 62`).
        (key.split_whitespace().last() == Some("CPU_Speed_Limit"))
            .then(|| value.trim().parse::<u8>().ok())?
    });
    match limit {
        Some(percent) if percent >= 100 => Thermal::Nominal,
        Some(percent) => Thermal::Limited(percent),
        None => Thermal::Unknown,
    }
}

#[cfg(windows)]
fn windows_reading() -> Reading {
    match windows_status() {
        Some((ac_line, battery_flag, system_status)) => {
            windows_reading_from(ac_line, battery_flag, system_status)
        }
        None => Reading::default(),
    }
}

/// `GetSystemPowerStatus`'s answer as a reading: `ACLineStatus` (1 on mains, 0 on the battery,
/// 255 for the API's own "unknown") and `SystemStatusFlag`'s first bit, the energy saver.
/// `BatteryFlag` says only whether there is a battery at all, which doesn't change what the
/// machine runs on: a desktop reads `ac`, as it does on the other two platforms. Built on every
/// platform so a test can check the parsing anywhere; only Windows calls it. Windows reports no
/// thermal state to a game.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_reading_from(ac_line: u8, _battery_flag: u8, system_status: u8) -> Reading {
    Reading {
        source: match ac_line {
            1 => Source::Ac,
            0 => Source::Battery,
            _ => Source::Unknown,
        },
        saver: system_status & 1 == 1,
        thermal: Thermal::Unknown,
    }
}

#[cfg(windows)]
fn windows_status() -> Option<(u8, u8, u8)> {
    /// `SYSTEM_POWER_STATUS`.
    #[repr(C)]
    struct PowerStatus {
        ac_line_status: u8,
        battery_flag: u8,
        battery_life_percent: u8,
        system_status_flag: u8,
        battery_life_time: u32,
        battery_full_life_time: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemPowerStatus(status: *mut PowerStatus) -> i32;
    }

    // The sentinels the API documents for "unknown", so a failed call reads as unknown.
    let mut status = PowerStatus {
        ac_line_status: 255,
        battery_flag: 0,
        battery_life_percent: 255,
        system_status_flag: 0,
        battery_life_time: u32::MAX,
        battery_full_life_time: u32::MAX,
    };
    // SAFETY: a live `SYSTEM_POWER_STATUS` with the documented layout; the call writes it and
    // fails returning 0, which leaves the sentinels above.
    (unsafe { GetSystemPowerStatus(&mut status) } != 0).then_some((
        status.ac_line_status,
        status.battery_flag,
        status.system_status_flag,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readings of one machine, as the OS would give them tick after tick.
    fn reading(source: Source) -> Reading {
        Reading {
            source,
            ..Reading::default()
        }
    }

    #[test]
    fn unplugging_and_plugging_back_announces_two_changes_and_no_repeats() {
        let mut channel = Channel::default();
        // Where the channel starts is not a change, even on the battery.
        channel.observe(reading(Source::Ac), false);
        assert_eq!(channel.transitions, 0);
        // The cable comes out, and the next ticks find the same thing.
        channel.observe(reading(Source::Battery), false);
        channel.observe(reading(Source::Battery), false);
        assert_eq!(channel.transitions, 1);
        // Back in: the second change, and once.
        channel.observe(reading(Source::Ac), false);
        channel.observe(reading(Source::Ac), false);
        assert_eq!(channel.transitions, 2);
        // The OS's own switches count as changes too, unlike a repeated reading.
        channel.observe(
            Reading {
                saver: true,
                ..reading(Source::Ac)
            },
            false,
        );
        assert_eq!(channel.transitions, 3);
    }

    #[test]
    fn a_machine_without_a_battery_announces_nothing() {
        // A desktop, or a platform with no probe: the OS names no source, and repeating that
        // is not a change however often it repeats.
        let mut channel = Channel::default();
        for _ in 0..5 {
            channel.observe(Reading::default(), false);
        }
        assert_eq!(channel.transitions, 0);
        // The same for the real reader's answer on a machine with a battery-less supply folder.
        let fixture = Fixture::new();
        fixture.supply("AC", "Mains", Some("0"));
        for _ in 0..3 {
            let source = power_supply_source(fixture.path());
            assert_eq!(source, Source::Unknown);
            channel.observe(reading(source), false);
        }
        assert_eq!(channel.transitions, 0);
    }

    #[test]
    fn a_sleep_between_two_ticks_counts_one_resume() {
        let start = Instant::now();
        let mut channel = Channel::default();
        let clock = Clock {
            monotonic: start,
            wall: 1_000_000,
        };
        // A 90-second gap: the machine slept, whatever the monotonic clock did meanwhile.
        let after = Clock {
            monotonic: start + Duration::from_secs(1),
            wall: 1_000_090,
        };
        channel.observe(reading(Source::Ac), clock.slept_to(after));
        assert_eq!(channel.resumes, 1);
        assert_eq!(channel.transitions, 1);
        // An ordinary tick: one second on both clocks, nothing to say.
        let even = Clock {
            monotonic: after.monotonic + TICK,
            wall: after.wall + 1,
        };
        assert!(!after.slept_to(even));
        channel.observe(reading(Source::Ac), after.slept_to(even));
        assert_eq!(channel.resumes, 1);
        // A wall clock set back an hour is a jump too: the distance counts, not the difference.
        let backwards = Clock {
            monotonic: even.monotonic + TICK,
            wall: even.wall - 3600,
        };
        assert!(even.slept_to(backwards));
    }

    #[test]
    fn the_sysfs_folder_says_mains_a_battery_or_neither() {
        let fixture = Fixture::new();
        fixture.supply("AC", "Mains", Some("1"));
        fixture.supply("BAT0", "Battery", None);
        // A USB supply is neither the mains nor the machine's battery.
        fixture.supply("ucsi-source-psy-1", "USB", None);
        assert_eq!(power_supply_source(fixture.path()), Source::Ac);
        // The cable out: the machine runs on the battery.
        fixture.write("AC/online", "0");
        assert_eq!(power_supply_source(fixture.path()), Source::Battery);
        // A folder the OS doesn't have, as on a platform without sysfs.
        assert_eq!(
            power_supply_source(&fixture.0.join("missing")),
            Source::Unknown
        );
    }

    #[test]
    fn pmset_names_the_power_it_draws_from() {
        assert_eq!(
            macos_source(
                "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t82%; discharging; 3:12 remaining present: true\n"
            ),
            Source::Battery
        );
        assert_eq!(
            macos_source(
                "Now drawing from 'AC Power'\n -InternalBattery-0 (id=1)\t100%; charged; 0:00 remaining present: true\n"
            ),
            Source::Ac
        );
        // A Mac without a battery still names the mains.
        assert_eq!(macos_source("Now drawing from 'AC Power'\n"), Source::Ac);
        // No output at all (`pmset` missing, or its answer empty).
        assert_eq!(macos_source(""), Source::Unknown);
    }

    #[test]
    fn pmset_says_how_far_the_cpu_is_held_back() {
        assert_eq!(
            macos_thermal("2026-11-19 15:00:00 +0300 CPU_Speed_Limit = 100\n"),
            Thermal::Nominal
        );
        assert_eq!(
            macos_thermal("2026-11-19 15:00:00 +0300 CPU_Speed_Limit = 62\n"),
            Thermal::Limited(62)
        );
        // Nothing recorded: macOS says so in a note, not in a limit.
        assert_eq!(
            macos_thermal(
                "Note: No thermal warning level has been recorded\nNote: No performance warning level has been recorded\n"
            ),
            Thermal::Unknown
        );
        assert_eq!(macos_thermal(""), Thermal::Unknown);
    }

    #[test]
    fn windows_reports_the_source_and_the_energy_saver() {
        // `ACLineStatus` 0: on the battery, and `SystemStatusFlag`'s first bit: the saver is on.
        let battery = windows_reading_from(0, 0, 1);
        assert_eq!(battery.source, Source::Battery);
        assert!(battery.saver);
        // On mains, saver off.
        let ac = windows_reading_from(1, 0, 0);
        assert_eq!(ac.source, Source::Ac);
        assert!(!ac.saver);
        // A desktop: `BatteryFlag`'s bit 7 says there is no battery, and it still draws from the
        // mains, so the source follows `ACLineStatus` and not the flag.
        assert_eq!(windows_reading_from(1, 128, 0).source, Source::Ac);
        // A laptop whose battery was pulled out runs on the mains the same way.
        assert_eq!(windows_reading_from(1, 128, 1).source, Source::Ac);
        // 255 is the API's own "I don't know", with or without a battery present.
        assert_eq!(windows_reading_from(255, 128, 0).source, Source::Unknown);
        assert_eq!(windows_reading_from(255, 0, 0).source, Source::Unknown);
        // Windows tells a game nothing about heat.
        assert_eq!(ac.thermal, Thermal::Unknown);
        assert_eq!(battery.thermal, Thermal::Unknown);
    }

    #[test]
    fn the_graphics_pages_warning_fires_on_the_saver_or_a_limited_cpu() {
        let saver = Sample {
            saver: true,
            ..Sample::default()
        };
        assert!(saver.limited());
        let hot = Sample {
            thermal: Thermal::Limited(62),
            ..Sample::default()
        };
        assert!(hot.limited());
        // A laptop on its battery with nothing else going on is not the OS holding it back.
        let on_battery = Sample {
            source: Source::Battery,
            ..Sample::default()
        };
        assert!(!on_battery.limited());
        assert!(!Sample::default().limited());
    }

    /// A minimal `/sys/class/power_supply` in a folder of its own, removed when dropped.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let name = format!(
                "horizon-power-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let mut dir = std::env::temp_dir().join(&name);
            if std::fs::create_dir_all(&dir).is_err() {
                // A test process confined to its build tree may not write to the user's temp
                // folder; the folder beside its own binary is always its own.
                let exe = std::env::current_exe().expect("the test binary's path");
                dir = exe.parent().expect("the test binary's folder").join(&name);
                std::fs::create_dir_all(&dir).expect("a folder for the fixture");
            }
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// One supply's folder: its `type`, and its `online` where the kernel has one.
        fn supply(&self, name: &str, kind: &str, online: Option<&str>) {
            let dir = self.0.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            if let Some(online) = online {
                self.write(&format!("{name}/online"), online);
            }
        }

        fn write(&self, file: &str, text: &str) {
            std::fs::write(self.0.join(file), text).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
