//! Python's `zoneinfo`, as far as the backend leans on it.
//!
//! Two questions are asked of it, and both have to be answered the way
//! the Python answers them *on the machine it runs on*:
//!
//! * [`is_available`] — `name in zoneinfo.available_timezones()`, which
//!   decides what `POST /api/settings/timezone` accepts. It is not a
//!   fixed list. It is the tzdata package's `zones` file plus a fresh
//!   walk of every TZPATH directory, keeping each file that begins
//!   `TZif`. On this repository's Debian image the walk adds
//!   `localtime`, a symlink to /etc/localtime, which the package does not
//!   list — so the Python there accepts it, and so must this.
//! * [`load`] — `ZoneInfo(key)`, which decides the rules a name resolves
//!   to. The first TZPATH directory holding the key *as a file* wins;
//!   only when none does is the package consulted. Production pairs
//!   Debian's tzdata (2026b) with the pip package (2026c), so for every
//!   ordinary name it is the system copy that decides.
//!
//! The package cannot be opened from Rust. Its name list is
//! `crate::tz_names`, generated from its `zones` file, and its zone data
//! is jiff's bundled database, which a test pins to the same IANA
//! release. jiff bundles the rearguard format: that changes which period
//! of a few zones (Europe/Dublin among them) is called daylight time,
//! never the total UTC offset, and the offset is all this module reads.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::NaiveDateTime;
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};

/// CPython's compiled-in TZPATH, which `PYTHONTZPATH` replaces.
const DEFAULT_TZPATH: &str = "/usr/share/zoneinfo:/usr/lib/zoneinfo:/usr/share/lib/zoneinfo:/etc/zoneinfo";

/// `zoneinfo.TZPATH`, fixed when the module is first used, as Python
/// fixes it at import.
fn tzpath() -> &'static [PathBuf] {
    static TZPATH: OnceLock<Vec<PathBuf>> = OnceLock::new();
    TZPATH.get_or_init(|| match std::env::var("PYTHONTZPATH") {
        Ok(raw) => parse_tzpath(&raw),
        Err(_) => parse_tzpath(DEFAULT_TZPATH),
    })
}

/// `_parse_python_tzpath`: empty means none at all, and relative
/// entries are dropped (with a warning Python prints and nothing reads).
fn parse_tzpath(raw: &str) -> Vec<PathBuf> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(':').filter(|p| p.starts_with('/')).map(PathBuf::from).collect()
}

/// `posixpath.normpath`.
fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial_slashes = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial_slashes == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial_slashes), comps.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// `_validate_tzfile_path`: a key must be relative, already normalised,
/// and stay beneath its root. Every refusal is a ValueError.
fn valid_key(key: &str) -> bool {
    if key.starts_with('/') {
        return false;
    }
    // normpath only ever removes ASCII, so comparing byte lengths is
    // comparing the code-point lengths Python compares.
    let norm = normpath(key);
    if norm.len() != key.len() {
        return false;
    }
    normpath(&format!("_/{norm}")).starts_with("_/")
}

/// `os.path.isfile`: follows symlinks, and is simply false for a path
/// the OS refuses (an embedded NUL included).
fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// The TZif magic, as both `available_timezones` and `load_data` test.
fn read_tzif(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    bytes.starts_with(b"TZif").then_some(bytes)
}

/// Why `ZoneInfo(key)` produced no zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// ZoneInfoNotFoundError or ValueError — the two a caller writing
    /// `except (ZoneInfoNotFoundError, ValueError)` catches.
    NotFound,
    /// IsADirectoryError, from a key naming a directory of the package.
    /// Nobody catches it; it becomes a 500.
    IsADirectory,
}

/// `ZoneInfo(key)`.
pub fn load(key: &str) -> Result<TimeZone, LoadError> {
    if !valid_key(key) {
        return Err(LoadError::NotFound);
    }
    // `find_tzfile`: the first root holding the key as a file.
    for root in tzpath() {
        let path = root.join(key);
        if is_file(&path) {
            // Found means committed: a file here that is not TZif is a
            // ValueError, not a reason to look further.
            let bytes = read_tzif(&path).ok_or(LoadError::NotFound)?;
            return TimeZone::tzif(key, &bytes).map_err(|_| LoadError::NotFound);
        }
    }
    load_tzdata(key)
}

/// `_common.load_tzdata`: the key's last component is a resource inside
/// the package `tzdata.zoneinfo.<the rest, joined with dots>`.
///
/// Joining with dots means a dotted directory reaches the same place as
/// a slashed one — `America.Argentina/Buenos_Aires` loads Buenos Aires
/// in Python, and does here. Opening a resource that is a directory is
/// the IsADirectoryError. Everything else that can go wrong in there is
/// an ImportError, FileNotFoundError or ValueError — all caught, all
/// [`LoadError::NotFound`]. What is not modelled is importing a module
/// that is not a zone directory, such as a `__pycache__` or
/// `__init__` component; none of those can load a zone.
fn load_tzdata(key: &str) -> Result<TimeZone, LoadError> {
    let (package, resource) = match key.rsplit_once('/') {
        Some((package, resource)) => (package.replace('/', "."), resource),
        None => (String::new(), key),
    };
    let mut parts: Vec<&str> = Vec::new();
    if !package.is_empty() {
        for part in package.split('.') {
            if part.is_empty() {
                return Err(LoadError::NotFound);
            }
            parts.push(part);
        }
    }
    parts.push(resource);
    let canonical = parts.join("/");

    if crate::tz_names::is_valid(&canonical) {
        return jiff::tz::TimeZoneDatabase::bundled()
            .get(&canonical)
            .map_err(|_| LoadError::NotFound);
    }
    if is_package_dir(&canonical) {
        return Err(LoadError::IsADirectory);
    }
    Err(LoadError::NotFound)
}

/// Whether `path` is a directory of the tzdata package: a proper prefix
/// of some zone name, ending at a `/`.
fn is_package_dir(path: &str) -> bool {
    let prefix = format!("{path}/");
    let names = crate::tz_names::NAMES;
    let at = names.partition_point(|n| *n < prefix.as_str());
    names.get(at).is_some_and(|n| n.starts_with(&prefix))
}

/// `name in zoneinfo.available_timezones()`.
pub fn is_available(name: &str) -> bool {
    if name == "posixrules" {
        return false;
    }
    if crate::tz_names::is_valid(name) {
        return true;
    }
    tzpath().iter().any(|root| walk_reaches(root, name))
}

/// Whether `available_timezones`' `os.walk` of `root` yields `key` as
/// a file whose first bytes are `TZif` — answered for the one key
/// rather than by walking thousands of files per request, as the Python
/// does.
///
/// The walk's rules, each of which a key has to satisfy:
/// * keys are `relpath`s — no empty, `.` or `..` components;
/// * `right/` and `posix/` are pruned at the top level only;
/// * `os.walk` does not descend into a symlinked directory;
/// * a directory entry, or a symlink to one, is never a file — but a
///   dangling symlink is, and then fails the TZif check.
fn walk_reaches(root: &Path, key: &str) -> bool {
    if !root.exists() || key.is_empty() || key.contains('\0') {
        return false;
    }
    let comps: Vec<&str> = key.split('/').collect();
    if comps.iter().any(|c| c.is_empty() || *c == "." || *c == "..") {
        return false;
    }
    if comps.len() > 1 && (comps[0] == "right" || comps[0] == "posix") {
        return false;
    }
    let mut dir = root.to_path_buf();
    for comp in &comps[..comps.len() - 1] {
        dir.push(comp);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            _ => return false,
        }
    }
    let path = dir.join(comps[comps.len() - 1]);
    if std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    read_tzif(&path).is_some()
}

/// The UTC offset Python gives `wall` in `tz` for a given `fold`.
///
/// PEP 495, as zoneinfo implements it: in a gap and in a fold alike,
/// `fold=0` takes the offset from before the transition and `fold=1`
/// the one after.
fn offset_for(tz: &TimeZone, wall: jiff::civil::DateTime, fold: bool) -> Offset {
    match tz.to_ambiguous_timestamp(wall).offset() {
        AmbiguousOffset::Unambiguous { offset } => offset,
        AmbiguousOffset::Gap { before, after } | AmbiguousOffset::Fold { before, after } => {
            if fold {
                after
            } else {
                before
            }
        }
    }
}

/// ```python
/// datetime.now(tz=tz).replace(hour=0, minute=0, second=0, microsecond=0)
///     .astimezone(UTC).replace(tzinfo=None)
/// ```
///
/// `replace` keeps `fold`, and `datetime.now(tz)` sets it when now is
/// the second pass through a repeated hour. So the fold of *now* picks
/// the offset of *midnight* — which matters only on a day whose
/// midnight is itself repeated, as it is in America/Havana.
pub fn local_midnight_utc(tz: &TimeZone, now: jiff::Timestamp) -> NaiveDateTime {
    let now_offset = tz.to_offset(now);
    let wall_now = now_offset.to_datetime(now);
    let fold = matches!(
        tz.to_ambiguous_timestamp(wall_now).offset(),
        AmbiguousOffset::Fold { after, .. } if after == now_offset
    );
    let midnight = wall_now.date().to_datetime(jiff::civil::Time::midnight());
    let offset = offset_for(tz, midnight, fold);
    let utc = midnight
        .to_zoned(TimeZone::fixed(offset))
        .map(|z| z.timestamp())
        .unwrap_or(now);
    let secs = utc.as_second();
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_matches_posixpath() {
        // Values from CPython 3.12's posixpath.normpath.
        for (input, want) in [
            ("", "."),
            (".", "."),
            ("a/../b", "b"),
            ("a/b/", "a/b"),
            ("a//b", "a/b"),
            ("../a", "../a"),
            ("/../a", "/a"),
            ("//a", "//a"),
            ("///a", "/a"),
            ("a/./b", "a/b"),
            ("_/..", "."),
            ("_/.", "_"),
        ] {
            assert_eq!(normpath(input), want, "{input:?}");
        }
    }

    #[test]
    fn keys_are_validated_like_zoneinfo() {
        for ok in ["UTC", "America/New_York", "a..b", "localtime", "right/UTC"] {
            assert!(valid_key(ok), "{ok:?}");
        }
        for bad in ["", ".", "..", "/etc/passwd", "../etc/passwd", "a/../b", "a/", "a//b", "a/./b"] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn package_directories_are_recognised() {
        assert!(is_package_dir("America"));
        assert!(is_package_dir("America/Argentina"));
        assert!(!is_package_dir("America/New_York"));
        assert!(!is_package_dir("Americ"));
        assert!(!is_package_dir("UTC"));
    }

    #[test]
    fn a_directory_key_is_the_uncaught_error() {
        assert_eq!(load("America").unwrap_err(), LoadError::IsADirectory);
        assert_eq!(load("America/Argentina").unwrap_err(), LoadError::IsADirectory);
        assert_eq!(load("Mars/Olympus").unwrap_err(), LoadError::NotFound);
        assert_eq!(load("UTC ").unwrap_err(), LoadError::NotFound);
        assert_eq!(load("../etc/passwd").unwrap_err(), LoadError::NotFound);
    }

    #[test]
    fn a_dotted_package_path_reaches_the_zone() {
        // Python joins the directories with dots to import them, so this
        // is Buenos Aires — unless a TZPATH directory has the key as a
        // file, which no system does.
        assert!(load("America.Argentina/Buenos_Aires").is_ok());
    }

    /// The bundled zones stand in for the pip package, so they must be
    /// the same release and hold every name the package lists.
    #[test]
    fn bundled_tzdb_is_the_packages_release() {
        assert_eq!(jiff_tzdb::VERSION, Some(crate::tz_names::TZDATA_VERSION));
        let db = jiff::tz::TimeZoneDatabase::bundled();
        let missing: Vec<_> = crate::tz_names::NAMES.iter().filter(|n| db.get(n).is_err()).collect();
        assert!(missing.is_empty(), "bundled tzdb lacks {missing:?}");
    }

    #[test]
    fn availability_answers_like_zoneinfo() {
        assert!(
            crate::tz_names::NAMES.windows(2).all(|w| w[0] < w[1]),
            "binary search needs sorted input"
        );
        // CPython 3.12's `name in available_timezones()` on the harness
        // host. `localtime` is left out: it is there on Debian and not on
        // Fedora, which is the point of walking at request time.
        for (name, want) in [
            ("UTC", true),
            ("America/Los_Angeles", true),
            ("Factory", true),
            ("utc", false),
            ("america/los_angeles", false),
            ("Mars/Olympus_Mons", false),
            ("", false),
            (" UTC", false),
            ("posixrules", false),
            ("right/UTC", false),
            ("posix/UTC", false),
            ("America", false),
            ("zone.tab", false),
            ("tzdata.zi", false),
        ] {
            assert_eq!(is_available(name), want, "{name:?}");
        }
    }

    /// The walk's rules, on a tree laid out like Debian's.
    #[test]
    fn the_walk_keeps_what_os_walk_keeps() {
        // Under target/, not /tmp: that is RAM here, and nothing else
        // in this crate writes there.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/zoneinfo-walk");
        let _ = std::fs::remove_dir_all(&root);
        let tzif = jiff_tzdb::get("UTC").unwrap().1;
        for dir in ["Etc", "right", "posix", "Linked/Real"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in ["Etc/UTC", "right/UTC", "posix/UTC", "Linked/Real/Zone"] {
            std::fs::write(root.join(file), tzif).unwrap();
        }
        std::fs::write(root.join("zone.tab"), "# not TZif\n").unwrap();
        std::os::unix::fs::symlink(root.join("Etc/UTC"), root.join("localtime")).unwrap();
        std::os::unix::fs::symlink(root.join("gone"), root.join("dangling")).unwrap();
        std::os::unix::fs::symlink(root.join("Linked/Real"), root.join("Via")).unwrap();

        for (key, want) in [
            ("Etc/UTC", true),
            ("localtime", true),         // a symlink to a file is a file
            ("right/UTC", false),        // pruned at the top
            ("posix/UTC", false),
            ("Linked/Real/Zone", true),
            ("Via/Zone", false),         // os.walk does not follow a linked dir
            ("dangling", false),         // listed, then fails the TZif check
            ("zone.tab", false),
            ("Etc", false),              // a directory is never a key
            ("Etc/../Etc/UTC", false),   // relpath never yields `..`
            ("Etc//UTC", false),
        ] {
            assert_eq!(walk_reaches(&root, key), want, "{key:?}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn at(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    fn naive(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn bundled(name: &str) -> TimeZone {
        jiff::tz::TimeZoneDatabase::bundled().get(name).unwrap()
    }

    /// Each expectation is what CPython 3.12 printed for the same zone
    /// and instant (tests/differential/midnight_probe.py).
    #[test]
    fn midnight_follows_pep_495() {
        for (zone, now, want) in [
            ("UTC", "2026-05-07T15:00:00Z", "2026-05-07 00:00:00"),
            ("America/Los_Angeles", "2026-05-07T15:00:00Z", "2026-05-07 07:00:00"),
            ("America/Los_Angeles", "2026-05-07T06:00:00Z", "2026-05-06 07:00:00"),
            ("Asia/Kolkata", "2026-05-07T20:00:00Z", "2026-05-07 18:30:00"),
            ("Pacific/Kiritimati", "2026-05-07T11:00:00Z", "2026-05-07 10:00:00"),
            // Santiago springs forward at midnight: 00:00 does not exist,
            // and fold=0 takes the offset from before the gap.
            ("America/Santiago", "2026-09-06T12:00:00Z", "2026-09-06 04:00:00"),
            // Havana falls back 01:00 -> 00:00, repeating midnight. Now in
            // the first pass: the first midnight. In the second: fold=1
            // carries over and midnight is the second one.
            ("America/Havana", "2026-11-01T04:30:00Z", "2026-11-01 04:00:00"),
            ("America/Havana", "2026-11-01T05:30:00Z", "2026-11-01 05:00:00"),
            ("America/Havana", "2026-11-01T12:00:00Z", "2026-11-01 04:00:00"),
        ] {
            let got = local_midnight_utc(&bundled(zone), at(now));
            assert_eq!(got, naive(want), "{zone} at {now}");
        }
    }
}
