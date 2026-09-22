//! Timestamps written in front of Terminal log lines.
//!
//! **The writer spells the format**, in the letters most Windows tools use:
//! `yyyy` `MM` `dd` `HH` `mm` `ss` `fff`. Everything else is copied as it
//! stands, so brackets and the space before the output are part of the format.

pub const DEFAULT_FORMAT: &str = "[yyyy-MM-dd HH:mm:ss] ";

/// A wall-clock time in the writer's own zone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

pub fn now() -> LocalTime {
    // SAFETY: GetLocalTime only fills in the structure it returns.
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    LocalTime {
        year: now.wYear,
        month: now.wMonth,
        day: now.wDay,
        hour: now.wHour,
        minute: now.wMinute,
        second: now.wSecond,
        millis: now.wMilliseconds,
    }
}

/// `format` with its letters replaced by `time`.
///
/// **Case matters**: `MM` is the month and `mm` the minute, as everywhere else
/// these letters are used.
pub fn format(format: &str, time: LocalTime) -> String {
    let fields: [(&str, u16, usize); 7] = [
        ("yyyy", time.year, 4),
        ("MM", time.month, 2),
        ("dd", time.day, 2),
        ("HH", time.hour, 2),
        ("mm", time.minute, 2),
        ("ss", time.second, 2),
        ("fff", time.millis, 3),
    ];
    let mut out = String::with_capacity(format.len() + 8);
    let mut rest = format;
    'next: while let Some(c) = rest.chars().next() {
        for (letters, value, width) in fields {
            if let Some(after) = rest.strip_prefix(letters) {
                out.push_str(&format!("{value:0width$}"));
                rest = after;
                continue 'next;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// What a log started with timestamps writes in front of every line.
#[derive(Clone, Debug)]
pub struct Stamp {
    format: String,
    clock: fn() -> LocalTime,
}

impl Stamp {
    pub fn new(format: &str) -> Self {
        let format = if format.is_empty() {
            DEFAULT_FORMAT
        } else {
            format
        };
        Self {
            format: format.to_owned(),
            clock: now,
        }
    }

    #[cfg(test)]
    pub fn at(format: &str, clock: fn() -> LocalTime) -> Self {
        Self {
            format: format.to_owned(),
            clock,
        }
    }

    pub fn text(&self) -> String {
        format(&self.format, (self.clock)())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIME: LocalTime = LocalTime {
        year: 2026,
        month: 9,
        day: 3,
        hour: 7,
        minute: 5,
        second: 9,
        millis: 42,
    };

    #[test]
    fn letters_are_replaced_and_everything_else_is_kept() {
        assert_eq!(format(DEFAULT_FORMAT, TIME), "[2026-09-03 07:05:09] ");
        assert_eq!(format("HH:mm:ss.fff | ", TIME), "07:05:09.042 | ");
        assert_eq!(format("yyyy年MM月dd日 ", TIME), "2026年09月03日 ");
        assert_eq!(format("mmMM", TIME), "0509", "case tells minute from month");
        assert_eq!(format("y M", TIME), "y M", "a lone letter is text");
        assert_eq!(format("", TIME), "");
    }

    #[test]
    fn an_empty_format_falls_back_to_the_default() {
        assert_eq!(Stamp::new("").format, DEFAULT_FORMAT);
    }
}
