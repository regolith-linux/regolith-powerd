//! One remembered power profile per power source.
//!
//! The user picks a power-profiles-daemon profile for when the machine runs
//! on AC and one for when it runs on battery, in Settings or with any other
//! client. Each pick is remembered for the power source in use, and the
//! remembered profile of a source is put back into effect when the machine
//! switches to it, when the daemon starts and when power-profiles-daemon
//! (re)appears. The first time on a source, the profile in effect is taken
//! as the pick.
//!
//! A profile hold always wins: power-profiles-daemon drops every hold when a
//! profile is set, so nothing is set while one is in place, and the profiles
//! that holds bring are not picks. When the last hold ends on a profile other
//! than the one in effect before the holds began, the user picked it (which
//! is what ended the holds), and that pick is remembered.
//!
//! The picks are saved under `$XDG_STATE_HOME/regolith-powerd/`, which both
//! sessions share. This module only decides; `profiles.rs` talks to the
//! services.

use glib::g_log;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::mem;
use std::path::{Path, PathBuf};

/// The profile put into effect when the remembered one is not offered.
const FALLBACK: &str = "balanced";
/// The profiles power-profiles-daemon knows.
const PROFILES: [&str; 3] = ["power-saver", "balanced", "performance"];

const STATE_DIR: &str = "regolith-powerd";
const STATE_FILE: &str = "profiles";
/// Where a new state file is written before it replaces the old one.
const PARTIAL_FILE: &str = "profiles.partial";
/// The first line of the state file, which names its format.
const HEADER: &str = "regolith-powerd profiles 1";

/// The power source, from UPower's `OnBattery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Ac,
    Battery,
}

impl Source {
    pub fn from_on_battery(on_battery: bool) -> Self {
        if on_battery {
            Self::Battery
        } else {
            Self::Ac
        }
    }
}

/// What power-profiles-daemon reports.
#[derive(Debug)]
pub struct Service {
    /// The unique bus name of the instance: a new one means it restarted.
    pub owner: String,
    pub active: String,
    /// How many holds are in place.
    pub holds: usize,
    pub offered: Vec<String>,
}

impl Service {
    fn offers(&self, profile: &str) -> bool {
        self.offered.iter().any(|p| p == profile)
    }
}

/// The remembered profiles, saved to the state directory on every change.
#[derive(Debug, Default)]
pub struct Store {
    /// Where the profiles are saved; without one they live in memory only.
    dir: Option<PathBuf>,
    ac: Option<String>,
    battery: Option<String>,
}

impl Store {
    /// The profiles saved in the state directory. A missing, unreadable or
    /// damaged state file remembers nothing.
    pub fn load() -> Self {
        state_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
            .map_or_else(Self::default, Self::load_from)
    }

    fn load_from(dir: PathBuf) -> Self {
        let (ac, battery) = fs::read(dir.join(STATE_FILE))
            .ok()
            .and_then(|contents| parse(&contents))
            .unwrap_or_default();
        Self {
            dir: Some(dir),
            ac,
            battery,
        }
    }

    pub fn get(&self, source: Source) -> Option<&str> {
        match source {
            Source::Ac => self.ac.as_deref(),
            Source::Battery => self.battery.as_deref(),
        }
    }

    /// Remember `profile` for `source`, and save it. A profile that cannot
    /// be saved is still remembered for as long as the daemon runs.
    pub fn set(&mut self, source: Source, profile: &str) {
        let slot = match source {
            Source::Ac => &mut self.ac,
            Source::Battery => &mut self.battery,
        };
        if slot.as_deref() == Some(profile) {
            return;
        }
        *slot = Some(profile.to_owned());
        if let Some(dir) = &self.dir {
            if let Err(e) = save(dir, &self.contents()) {
                g_log!(
                    glib::LogLevel::Warning,
                    "Cannot save the power profiles in {}: {e}",
                    dir.display()
                );
            }
        }
    }

    fn contents(&self) -> String {
        format!(
            "{HEADER}\nac={}\nbattery={}\n",
            self.ac.as_deref().unwrap_or_default(),
            self.battery.as_deref().unwrap_or_default()
        )
    }
}

/// The state directory: `$XDG_STATE_HOME/regolith-powerd`, or
/// `$HOME/.local/state/regolith-powerd` when `XDG_STATE_HOME` is unset (or,
/// as the XDG base directory specification says, empty or relative).
fn state_dir(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let state_home = match state_home.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => PathBuf::from(home?).join(".local/state"),
    };
    Some(state_home.join(STATE_DIR))
}

/// Parse a state file. It must be whole: the header, the AC line and the
/// battery line, each ending in a newline and naming a known profile or
/// nothing. Anything else, a cut-short file included, is damaged.
fn parse(contents: &[u8]) -> Option<(Option<String>, Option<String>)> {
    let mut lines = std::str::from_utf8(contents)
        .ok()?
        .strip_suffix('\n')?
        .split('\n');
    if lines.next()? != HEADER {
        return None;
    }
    let ac = parse_profile(lines.next()?.strip_prefix("ac=")?)?;
    let battery = parse_profile(lines.next()?.strip_prefix("battery=")?)?;
    lines.next().is_none().then_some((ac, battery))
}

/// `Some(None)` when nothing is remembered, `None` for an unknown profile.
fn parse_profile(value: &str) -> Option<Option<String>> {
    match value {
        "" => Some(None),
        known if PROFILES.contains(&known) => Some(Some(known.to_owned())),
        _ => None,
    }
}

/// Replace the state file with `contents`, so that a crash at any point
/// leaves either the old file or the new one, never a mix: the new file is
/// synced before it replaces the old one, and the directory after.
fn save(dir: &Path, contents: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let partial = dir.join(PARTIAL_FILE);
    let result = fs::File::create(&partial)
        .and_then(|mut file| {
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&partial, dir.join(STATE_FILE)));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result?;
    fs::File::open(dir)?.sync_all()
}

/// The remembered profiles, and what is known of power-profiles-daemon.
#[derive(Debug)]
pub struct Remembered {
    store: Store,
    /// The service as last read, while it runs.
    service: Option<Service>,
    /// The profile in effect just before the holds in place began; `None`
    /// when they were in place already when the service was first read.
    before_holds: Option<String>,
}

impl Remembered {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            service: None,
            before_holds: None,
        }
    }

    /// Take in a new reading of the service (`None` when it does not run),
    /// made on `source`, and remember the pick it shows, if any.
    pub fn observe(&mut self, source: Source, service: Option<Service>) {
        let previous = mem::replace(&mut self.service, service);
        let Some(current) = &self.service else {
            return;
        };
        let picked = match previous {
            Some(previous) if previous.owner == current.owner => {
                picked(&previous, current, &mut self.before_holds)
            }
            // The service appeared or restarted, on the profile it had chosen
            // last, which is not a pick. Holds in place began unseen.
            _ => {
                self.before_holds = None;
                None
            }
        };
        // The first time on a source, the profile in effect is the pick.
        let first = current.holds == 0 && self.store.get(source).is_none();
        if let Some(profile) = picked.or(first.then_some(current.active.as_str())) {
            self.store.set(source, profile);
        }
    }

    /// The profile to put into effect on `source`: the remembered one, or
    /// `balanced` when the service does not offer it. Nothing while a hold
    /// is in place, or when it is in effect already.
    pub fn wanted(&self, source: Source) -> Option<&str> {
        let service = self.service.as_ref().filter(|s| s.holds == 0)?;
        let remembered = self.store.get(source)?;
        let profile = [remembered, FALLBACK]
            .into_iter()
            .find(|p| service.offers(p))?;
        (profile != service.active).then_some(profile)
    }

    /// `profile` was put into effect, so the change is not a pick.
    pub fn applied(&mut self, profile: &str) {
        if let Some(service) = &mut self.service {
            service.active = profile.to_owned();
        }
    }
}

/// The pick that the change from `previous` to `current` shows, if any.
/// `before_holds` follows the profile in effect before the holds began.
fn picked<'a>(
    previous: &Service,
    current: &'a Service,
    before_holds: &mut Option<String>,
) -> Option<&'a str> {
    if previous.holds == 0 && current.holds > 0 {
        *before_holds = Some(previous.active.clone());
    }
    // A change while a hold is in place comes from the holds.
    if current.holds > 0 {
        return None;
    }
    let before_holds = before_holds.take();
    if current.active == previous.active {
        return None;
    }
    // Holds that end on their own return to the profile in effect before
    // them. Any other profile is a pick, which ended the holds.
    let ended_on_their_own = match before_holds {
        Some(profile) => profile == current.active,
        // In place before the service was read: whatever they end on.
        None => true,
    };
    if previous.holds > 0 && ended_on_their_own {
        return None;
    }
    Some(&current.active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Source::{Ac, Battery};

    /// Remembered profiles in memory, with nothing read of the service yet.
    fn remembered(ac: Option<&str>, battery: Option<&str>) -> Remembered {
        Remembered::new(Store {
            dir: None,
            ac: ac.map(Into::into),
            battery: battery.map(Into::into),
        })
    }

    impl Remembered {
        /// Observe the instance `owner` on `source` with `active` in effect
        /// and `holds` holds, offering `offered`.
        fn read(
            &mut self,
            source: Source,
            owner: &str,
            active: &str,
            holds: usize,
            offered: &[&str],
        ) {
            let service = Service {
                owner: owner.into(),
                active: active.into(),
                holds,
                offered: offered.iter().map(|p| p.to_string()).collect(),
            };
            self.observe(source, Some(service));
        }

        /// Observe the first instance, which offers every profile.
        fn see(&mut self, source: Source, active: &str, holds: usize) {
            self.read(source, ":1.1", active, holds, &PROFILES);
        }

        fn picks(&self) -> (Option<&str>, Option<&str>) {
            (self.store.get(Ac), self.store.get(Battery))
        }
    }

    /// A scratch directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "regolith-powerd-test-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn state_dir_follows_the_xdg_base_directory_specification() {
        let dir = |state: Option<&str>, home: Option<&str>| {
            state_dir(state.map(Into::into), home.map(Into::into))
        };
        let fallback = Some(PathBuf::from("/home/u/.local/state/regolith-powerd"));
        assert_eq!(
            dir(Some("/state"), Some("/home/u")),
            Some(PathBuf::from("/state/regolith-powerd"))
        );
        assert_eq!(dir(None, Some("/home/u")), fallback);
        assert_eq!(dir(Some(""), Some("/home/u")), fallback);
        assert_eq!(dir(Some("state"), Some("/home/u")), fallback);
        assert_eq!(dir(None, None), None);
    }

    #[test]
    fn store_survives_a_reload_and_makes_its_directory() {
        let scratch = Scratch::new("reload");
        let dir = scratch.0.join("state/regolith-powerd");
        let mut store = Store::load_from(dir.clone());
        assert_eq!(store.get(Ac), None);
        store.set(Battery, "power-saver");
        assert_eq!(Store::load_from(dir.clone()).get(Ac), None);
        store.set(Ac, "performance");
        let reloaded = Store::load_from(dir.clone());
        assert_eq!(reloaded.get(Ac), Some("performance"));
        assert_eq!(reloaded.get(Battery), Some("power-saver"));
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [STATE_FILE]);
    }

    #[test]
    fn damaged_state_remembers_nothing() {
        let whole = format!("{HEADER}\nac=performance\nbattery=power-saver\n");
        let picks = |ac: Option<&str>, battery: Option<&str>| {
            Some((ac.map(Into::into), battery.map(Into::into)))
        };
        assert_eq!(
            parse(whole.as_bytes()),
            picks(Some("performance"), Some("power-saver"))
        );
        assert_eq!(
            parse(format!("{HEADER}\nac=\nbattery=balanced\n").as_bytes()),
            picks(None, Some("balanced"))
        );
        for cut in 0..whole.len() {
            assert_eq!(parse(&whole.as_bytes()[..cut]), None, "cut at {cut}");
        }
        for damaged in [
            "\u{fffd}".as_bytes(),
            b"\xff\xfe\x80",
            format!("{HEADER}\nac=turbo\nbattery=balanced\n").as_bytes(),
            format!("{HEADER}\nbattery=balanced\nac=balanced\n").as_bytes(),
            format!("{HEADER}\nac=balanced\nbattery=balanced\n\n").as_bytes(),
            b"ac=balanced\nbattery=balanced\n",
        ] {
            assert_eq!(parse(damaged), None, "{damaged:?}");
        }
    }

    #[test]
    fn damaged_or_unwritable_state_still_remembers() {
        let scratch = Scratch::new("damaged");
        let dir = scratch.0.join("regolith-powerd");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(STATE_FILE), b"\xff\xfe\x80").unwrap();
        let mut store = Store::load_from(dir.clone());
        assert_eq!(store.get(Ac), None);
        store.set(Ac, "balanced");
        assert_eq!(Store::load_from(dir).get(Ac), Some("balanced"));

        // No state directory can be made under a file.
        let file = scratch.0.join("file");
        fs::write(&file, "").unwrap();
        let mut store = Store::load_from(file.join("regolith-powerd"));
        store.set(Battery, "power-saver");
        assert_eq!(store.get(Battery), Some("power-saver"));
    }

    #[test]
    fn learns_picks_and_switches_with_the_source() {
        let mut remembered = remembered(None, None);
        // The first time on each source, the profile in effect is taken.
        remembered.see(Ac, "balanced", 0);
        assert_eq!(remembered.picks(), (Some("balanced"), None));
        remembered.see(Ac, "performance", 0);
        remembered.see(Battery, "performance", 0);
        assert_eq!(remembered.wanted(Battery), None);
        remembered.see(Battery, "power-saver", 0);
        assert_eq!(
            remembered.picks(),
            (Some("performance"), Some("power-saver"))
        );

        // Plugged in: the AC pick comes back, and setting it is no pick.
        remembered.see(Ac, "power-saver", 0);
        assert_eq!(remembered.wanted(Ac), Some("performance"));
        remembered.applied("performance");
        remembered.see(Ac, "performance", 0);
        assert_eq!(remembered.wanted(Ac), None);
        assert_eq!(
            remembered.picks(),
            (Some("performance"), Some("power-saver"))
        );
    }

    #[test]
    fn falls_back_to_balanced_and_keeps_the_pick() {
        let mut remembered = remembered(Some("performance"), Some("power-saver"));
        let offered = ["power-saver", "balanced"];
        remembered.read(Ac, ":1.1", "power-saver", 0, &offered);
        assert_eq!(remembered.wanted(Ac), Some("balanced"));
        remembered.applied("balanced");
        remembered.read(Ac, ":1.1", "balanced", 0, &offered);
        assert_eq!(remembered.wanted(Ac), None);
        assert_eq!(
            remembered.picks(),
            (Some("performance"), Some("power-saver"))
        );
        // Back with performance offered again.
        remembered.observe(Ac, None);
        remembered.read(Ac, ":1.2", "balanced", 0, &PROFILES);
        assert_eq!(remembered.wanted(Ac), Some("performance"));
    }

    #[test]
    fn a_restart_is_no_pick() {
        let mut remembered = remembered(Some("performance"), Some("power-saver"));
        remembered.see(Ac, "performance", 0);
        // A new instance, read without the old one seen gone.
        remembered.read(Ac, ":1.2", "balanced", 0, &PROFILES);
        assert_eq!(
            remembered.picks(),
            (Some("performance"), Some("power-saver"))
        );
        assert_eq!(remembered.wanted(Ac), Some("performance"));
    }

    #[test]
    fn nothing_is_set_or_learned_under_a_hold() {
        let mut remembered = remembered(Some("balanced"), Some("power-saver"));
        remembered.see(Ac, "balanced", 0);
        remembered.see(Ac, "performance", 1);
        // Unplugged, and a second hold changes the profile again.
        remembered.see(Battery, "performance", 1);
        assert_eq!(remembered.wanted(Battery), None);
        remembered.see(Battery, "power-saver", 2);
        remembered.see(Battery, "performance", 1);
        assert_eq!(remembered.wanted(Battery), None);
        // The last hold ends on its own: the battery pick comes back.
        remembered.see(Battery, "balanced", 0);
        assert_eq!(remembered.picks(), (Some("balanced"), Some("power-saver")));
        assert_eq!(remembered.wanted(Battery), Some("power-saver"));
    }

    #[test]
    fn a_pick_that_ends_a_hold_is_remembered() {
        let mut remembered = remembered(Some("balanced"), Some("power-saver"));
        remembered.see(Battery, "power-saver", 0);
        remembered.see(Battery, "performance", 1);
        remembered.see(Battery, "balanced", 0);
        assert_eq!(remembered.picks(), (Some("balanced"), Some("balanced")));
        assert_eq!(remembered.wanted(Battery), None);
    }

    #[test]
    fn holds_in_place_when_first_read_restore_the_pick_when_they_end() {
        let mut remembered = remembered(Some("balanced"), None);
        remembered.see(Ac, "power-saver", 1);
        assert_eq!(remembered.wanted(Ac), None);
        remembered.see(Ac, "performance", 0);
        assert_eq!(remembered.picks(), (Some("balanced"), None));
        assert_eq!(remembered.wanted(Ac), Some("balanced"));
    }

    #[test]
    fn the_first_time_on_a_source_waits_for_the_holds_to_end() {
        let mut remembered = remembered(Some("balanced"), None);
        remembered.see(Ac, "balanced", 0);
        remembered.see(Ac, "power-saver", 1);
        remembered.see(Battery, "power-saver", 1);
        assert_eq!(remembered.picks(), (Some("balanced"), None));
        remembered.see(Battery, "balanced", 0);
        assert_eq!(remembered.picks(), (Some("balanced"), Some("balanced")));
        assert_eq!(remembered.wanted(Battery), None);
    }
}
