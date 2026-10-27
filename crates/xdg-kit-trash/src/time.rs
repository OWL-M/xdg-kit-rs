//! `DeletionDate`: a civil date and time in some local time zone, which
//! the file does not name.

use std::fmt;
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};

/// The time zone that deletion times are written in, and read back with.
///
/// The spec asks for the user's (or the file system's) local time, without
/// saying which zone that was. [`LocalZone::system`] finds the system's
/// zone; nothing else in this crate reads the environment for it.
#[derive(Debug, Clone)]
pub struct LocalZone(TimeZone);

/// Why a time zone could not be had.
#[derive(Debug)]
pub struct ZoneError(jiff::Error);

impl fmt::Display for ZoneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no usable time zone: {}", self.0)
    }
}

impl std::error::Error for ZoneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl LocalZone {
    /// Coordinated Universal Time.
    #[must_use]
    pub fn utc() -> Self {
        Self(TimeZone::UTC)
    }

    /// A fixed offset east of UTC, in seconds; `None` beyond ±25:59:59.
    #[must_use]
    pub fn fixed(seconds: i32) -> Option<Self> {
        Offset::from_seconds(seconds)
            .ok()
            .map(|offset| Self(TimeZone::fixed(offset)))
    }

    /// A zone from `TZif` data (the format of `/usr/share/zoneinfo`), with
    /// its name for messages.
    ///
    /// # Errors
    ///
    /// When `data` is not valid `TZif`.
    pub fn tzif(name: &str, data: &[u8]) -> Result<Self, ZoneError> {
        TimeZone::tzif(name, data).map(Self).map_err(ZoneError)
    }

    /// The system's zone, by jiff's detection: `TZ` when set (a zone name,
    /// a `TZif` path or a POSIX rule), else `/etc/localtime` (on Android, the
    /// `persist.sys.timezone` property). This reads the environment and the
    /// file system.
    ///
    /// # Errors
    ///
    /// When `TZ` is set to something unusable, or no system zone is found.
    pub fn system() -> Result<Self, ZoneError> {
        TimeZone::try_system().map(Self).map_err(ZoneError)
    }

    /// The zone's name, when it has one (IANA zones and `TZif` data given a
    /// name do).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.0.iana_name()
    }
}

/// Where the time of trashing comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clock {
    /// The system clock, read when the trash info file is written.
    #[default]
    System,
    /// Always this time.
    Fixed(SystemTime),
}

impl Clock {
    pub(crate) fn now(self) -> SystemTime {
        match self {
            Self::System => SystemTime::now(),
            Self::Fixed(at) => at,
        }
    }
}

/// A `DeletionDate`: a civil date and time, `YYYY-MM-DDThh:mm:ss`, in a
/// local time zone the file does not record. On its own it is not an
/// instant; [`DeletionDate::instants`] says which instants it can be in a
/// given zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeletionDate(DateTime);

/// The instants a civil time can be in a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instants {
    /// Exactly one.
    Unique(SystemTime),
    /// Two: the time was on the clock twice (when clocks went back).
    Ambiguous {
        /// Read with the offset before the change.
        earlier: SystemTime,
        /// Read with the offset after it.
        later: SystemTime,
    },
    /// Not representable: an instant it can be is past the instants this
    /// crate converts (jiff's, which end at 9999-12-30T22:00:00Z).
    OutOfRange,
    /// None: the time was skipped (when clocks went forward). These are
    /// the instants it means read with the offsets on either side.
    Skipped {
        /// Read with the offset before the gap.
        before: SystemTime,
        /// Read with the offset after it.
        after: SystemTime,
    },
}

impl DeletionDate {
    /// The civil date and time; `None` when it is no valid date or time
    /// (no leap seconds) or the year is outside 0–9999, which the format's
    /// four digits hold.
    #[must_use]
    pub fn new(year: i16, month: i8, day: i8, hour: i8, minute: i8, second: i8) -> Option<Self> {
        if !(0..=9999).contains(&year) {
            return None;
        }
        DateTime::new(year, month, day, hour, minute, second, 0)
            .ok()
            .map(Self)
    }

    /// The civil time on the clock of `zone` at `at`, to the second (the
    /// fraction dropped); `None` outside the years 0–9999.
    #[must_use]
    pub fn at(at: SystemTime, zone: &LocalZone) -> Option<Self> {
        let instant = Timestamp::try_from(at).ok()?;
        let civil = zone.0.to_datetime(instant);
        Self::new(
            civil.year(),
            civil.month(),
            civil.day(),
            civil.hour(),
            civil.minute(),
            civil.second(),
        )
    }

    /// Parses a value: `YYYY-MM-DDThh:mm:ss` exactly, which the spec
    /// requires and implementations write. The basic date form of the
    /// spec's own example, `YYYYMMDDThh:mm:ss`, is read too. A fraction or
    /// a zone is not.
    #[must_use]
    pub fn parse(text: &[u8]) -> Option<Self> {
        let number = |range: std::ops::Range<usize>| -> Option<i16> {
            let digits = text.get(range)?;
            let mut value = 0i16;
            for &digit in digits {
                if !digit.is_ascii_digit() {
                    return None;
                }
                value = value * 10 + i16::from(digit - b'0');
            }
            Some(value)
        };
        let small = |range| number(range).and_then(|value| i8::try_from(value).ok());
        let at = |index: usize, byte: u8| text.get(index) == Some(&byte);
        let (date_len, month, day) = if at(4, b'-') {
            if !at(7, b'-') {
                return None;
            }
            (10, small(5..7)?, small(8..10)?)
        } else {
            (8, small(4..6)?, small(6..8)?)
        };
        let time = date_len + 1;
        if text.len() != time + 8
            || !at(date_len, b'T')
            || !at(time + 2, b':')
            || !at(time + 5, b':')
        {
            return None;
        }
        Self::new(
            number(0..4)?,
            month,
            day,
            small(time..time + 2)?,
            small(time + 3..time + 5)?,
            small(time + 6..time + 8)?,
        )
    }

    /// The year.
    #[must_use]
    pub fn year(self) -> i16 {
        self.0.year()
    }

    /// The month, 1–12.
    #[must_use]
    pub fn month(self) -> i8 {
        self.0.month()
    }

    /// The day of the month, from 1.
    #[must_use]
    pub fn day(self) -> i8 {
        self.0.day()
    }

    /// The hour, 0–23.
    #[must_use]
    pub fn hour(self) -> i8 {
        self.0.hour()
    }

    /// The minute, 0–59.
    #[must_use]
    pub fn minute(self) -> i8 {
        self.0.minute()
    }

    /// The second, 0–59.
    #[must_use]
    pub fn second(self) -> i8 {
        self.0.second()
    }

    /// The instants this civil time can be in `zone`. Whether `zone` is
    /// the one it was written in is not known: the file does not say.
    #[must_use]
    pub fn instants(self, zone: &LocalZone) -> Instants {
        // Near either end of the years 0–9999, a civil time can be outside
        // the instants a `Timestamp` holds (they end at
        // 9999-12-30T22:00:00Z): then it is out of range, not some other
        // instant.
        let with = |offset: Offset| offset.to_timestamp(self.0).ok().map(SystemTime::from);
        let instants = match zone.0.to_ambiguous_timestamp(self.0).offset() {
            AmbiguousOffset::Unambiguous { offset } => with(offset).map(Instants::Unique),
            AmbiguousOffset::Fold { before, after } => with(before)
                .zip(with(after))
                .map(|(earlier, later)| Instants::Ambiguous { earlier, later }),
            AmbiguousOffset::Gap { before, after } => with(before)
                .zip(with(after))
                .map(|(before, after)| Instants::Skipped { before, after }),
        };
        instants.unwrap_or(Instants::OutOfRange)
    }
}

impl fmt::Display for DeletionDate {
    /// `YYYY-MM-DDThh:mm:ss`, as written to trash info files.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year(),
            self.month(),
            self.day(),
            self.hour(),
            self.minute(),
            self.second()
        )
    }
}
