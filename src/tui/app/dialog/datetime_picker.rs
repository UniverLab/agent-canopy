//! Shared inline date-time picker (year/month/day/hour/minute), extracted
//! from the prompt builder's scheduled-send control (U11, `prompt.rs`'s
//! former `SendAtEdit`) so the loop autorun dialog (C18) can reuse the exact
//! same widget instead of building a second one — see `loop_control.rs`'s
//! `LoopAutorunDialog`.

use chrono::Timelike;

/// Resolve a picked local wall-clock value to its UTC instant, leniently:
/// DST's "fall back" hour is ambiguous (two UTC instants map to the same
/// local time) and its "spring forward" hour doesn't exist at all. Both are
/// resolved to the earliest sane instant rather than surfacing the ambiguity
/// to the user — mirrors `schedule_send_prompt`'s handling for the prompt
/// builder's own send-at picker.
pub fn local_naive_to_utc(value: chrono::NaiveDateTime) -> chrono::DateTime<chrono::Utc> {
    value
        .and_local_timezone(chrono::Local)
        .earliest()
        .unwrap_or_else(chrono::Local::now)
        .with_timezone(&chrono::Utc)
}

/// Inline date-time picker state, edited one field at a time. Local
/// wall-clock throughout — callers convert to UTC only at the point they
/// act on the picked value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTimeEdit {
    pub value: chrono::NaiveDateTime,
    /// Focused field: 0=year, 1=month, 2=day, 3=hour, 4=minute.
    pub field: usize,
    /// In-progress numeric accumulator for the focused field while the user
    /// is typing digits: the running value and how many digits have been
    /// entered since the field was (re)started. Reset on field change and on
    /// arrow adjust so a fresh digit always starts a new number.
    pub typed: u32,
    pub typed_len: u8,
}

/// Digits a field accepts before it is "full" and auto-advances: the year is
/// 4 digits wide, every other field is 2.
pub fn field_digit_width(field: usize) -> u8 {
    if field == 0 {
        4
    } else {
        2
    }
}

/// Month arithmetic for the picker: ±N months with day clamping (chrono's
/// checked add/sub semantics — Jan 31 + 1 month = Feb 28/29).
pub fn add_months(value: chrono::NaiveDateTime, delta: i64) -> Option<chrono::NaiveDateTime> {
    if delta >= 0 {
        value.checked_add_months(chrono::Months::new(delta as u32))
    } else {
        value.checked_sub_months(chrono::Months::new(delta.unsigned_abs() as u32))
    }
}

/// Number of days in a given month (handles leap years).
pub fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    match (
        chrono::NaiveDate::from_ymd_opt(year, month, 1),
        chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1),
    ) {
        (Some(first), Some(next)) => (next - first).num_days() as u32,
        _ => 28,
    }
}

/// Set one component (year/month/day/hour/minute) of `value` to `num`,
/// clamping it into that component's valid range and clamping the day to the
/// resulting month length. Returns `None` only if chrono still rejects the
/// date, in which case the caller keeps the previous value.
pub fn with_field(
    value: chrono::NaiveDateTime,
    field: usize,
    num: u32,
) -> Option<chrono::NaiveDateTime> {
    use chrono::{Datelike, NaiveDate};
    let date = value.date();
    let time = value.time();
    match field {
        0 => {
            let year = num.clamp(1, 9999) as i32;
            let day = date.day().min(days_in_month(year, date.month()));
            NaiveDate::from_ymd_opt(year, date.month(), day).map(|d| d.and_time(time))
        }
        1 => {
            let month = num.clamp(1, 12);
            let day = date.day().min(days_in_month(date.year(), month));
            NaiveDate::from_ymd_opt(date.year(), month, day).map(|d| d.and_time(time))
        }
        2 => {
            let day = num.clamp(1, days_in_month(date.year(), date.month()));
            NaiveDate::from_ymd_opt(date.year(), date.month(), day).map(|d| d.and_time(time))
        }
        3 => {
            let hour = num.min(23);
            date.and_hms_opt(hour, time.minute(), time.second())
        }
        _ => {
            let minute = num.min(59);
            date.and_hms_opt(time.hour(), minute, time.second())
        }
    }
}

impl DateTimeEdit {
    /// Seed a fresh picker at `seed`, focused on the year field.
    pub fn new(seed: chrono::NaiveDateTime) -> Self {
        Self {
            value: seed,
            field: 0,
            typed: 0,
            typed_len: 0,
        }
    }

    /// Move the focused field (0=year … 4=minute), clamped to that range.
    /// Changing field clears any half-typed number so the next digit starts
    /// fresh.
    pub fn move_field(&mut self, delta: isize) {
        let next = self.field as isize + delta;
        self.field = next.clamp(0, 4) as usize;
        self.typed = 0;
        self.typed_len = 0;
    }

    /// Adjust the focused field by `delta` with real calendar math
    /// (months/days carry correctly). Arrows and typing coexist: an arrow
    /// clears the digit accumulator so a following digit starts a new
    /// number.
    pub fn adjust(&mut self, delta: i64) {
        self.typed = 0;
        self.typed_len = 0;
        let adjusted = match self.field {
            0 => add_months(self.value, delta * 12),
            1 => add_months(self.value, delta),
            2 => Some(self.value + chrono::Duration::days(delta)),
            3 => Some(self.value + chrono::Duration::hours(delta)),
            _ => Some(self.value + chrono::Duration::minutes(delta)),
        };
        if let Some(adjusted) = adjusted {
            self.value = adjusted;
        }
    }

    /// Type a digit into the focused field. Digits accumulate within the
    /// field (year 4 wide, others 2); when the field fills it auto-advances
    /// to the next field, and a digit typed into an already-full field
    /// restarts that field. The resulting component is clamped into its
    /// valid range on the fly (e.g. month `13` → `12`), mirroring the
    /// clamping [`Self::adjust`] already performs.
    pub fn type_digit(&mut self, digit: u32) {
        let field = self.field;
        let width = field_digit_width(field);
        // A digit landing on an already-full field starts the number over.
        if self.typed_len >= width {
            self.typed = 0;
            self.typed_len = 0;
        }
        self.typed = self.typed * 10 + digit;
        self.typed_len += 1;
        if let Some(updated) = with_field(self.value, field, self.typed) {
            self.value = updated;
        }
        // Field full → auto-advance to the next field (clamped at minute).
        if self.typed_len >= width {
            self.field = (field + 1).min(4);
            self.typed = 0;
            self.typed_len = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_seeds_value_and_starts_at_year_field() {
        let seed = chrono::NaiveDate::from_ymd_opt(2026, 8, 20)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let edit = DateTimeEdit::new(seed);
        assert_eq!(edit.value, seed);
        assert_eq!(edit.field, 0);
        assert_eq!(edit.typed, 0);
        assert_eq!(edit.typed_len, 0);
    }

    #[test]
    fn move_field_clamps_to_valid_range() {
        let mut edit = DateTimeEdit::new(chrono::NaiveDateTime::default());
        edit.move_field(10);
        assert_eq!(edit.field, 4);
        edit.move_field(-100);
        assert_eq!(edit.field, 0);
    }

    // ── local_naive_to_utc (C18) ────────────────────────────────────
    //
    // Both tests pin `TZ` for the duration of the conversion call and
    // restore it immediately after, since `local_naive_to_utc` reads the
    // process's local timezone. `TZ_TEST_LOCK` serializes the two of them
    // against each other so their mutations of the shared process-wide `TZ`
    // env var can't interleave (they did, and produced a wrong result,
    // before this lock was added).
    static TZ_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn local_to_utc_applies_the_zones_offset() {
        let _guard = TZ_TEST_LOCK.lock().unwrap();
        let original = std::env::var("TZ").ok();
        std::env::set_var("TZ", "America/Bogota"); // UTC-05:00, no DST
        let local = chrono::NaiveDate::from_ymd_opt(2026, 6, 15)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let utc = local_naive_to_utc(local);
        match original {
            Some(tz) => std::env::set_var("TZ", tz),
            None => std::env::remove_var("TZ"),
        }
        assert_eq!(
            utc,
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                chrono::NaiveDate::from_ymd_opt(2026, 6, 15)
                    .unwrap()
                    .and_hms_opt(14, 0, 0)
                    .unwrap(),
                chrono::Utc
            )
        );
    }

    #[test]
    fn local_to_utc_crosses_a_day_boundary() {
        let _guard = TZ_TEST_LOCK.lock().unwrap();
        let original = std::env::var("TZ").ok();
        std::env::set_var("TZ", "Pacific/Kiritimati"); // UTC+14:00, no DST
        let local = chrono::NaiveDate::from_ymd_opt(2026, 3, 5)
            .unwrap()
            .and_hms_opt(0, 30, 0)
            .unwrap();
        let utc = local_naive_to_utc(local);
        match original {
            Some(tz) => std::env::set_var("TZ", tz),
            None => std::env::remove_var("TZ"),
        }
        assert_eq!(
            utc.date_naive(),
            chrono::NaiveDate::from_ymd_opt(2026, 3, 4).unwrap()
        );
        assert_eq!(
            utc.time(),
            chrono::NaiveTime::from_hms_opt(10, 30, 0).unwrap()
        );
    }

    #[test]
    fn type_digit_auto_advances_when_field_fills() {
        let mut edit = DateTimeEdit::new(chrono::NaiveDateTime::default());
        edit.field = 3; // hour (width 2)
        edit.type_digit(1);
        assert_eq!(edit.field, 3);
        edit.type_digit(4);
        assert_eq!(edit.value.hour(), 14);
        assert_eq!(edit.field, 4);
        assert_eq!(edit.typed, 0);
    }
}
