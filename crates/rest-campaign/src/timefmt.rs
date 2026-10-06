//! Istanti UTC in RFC 3339, senza dipendenze: servono per le deadline del
//! contratto e per le date del report.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `SystemTime` in RFC 3339 UTC con millisecondi (`2026-10-05T12:34:56.789Z`).
///
/// Un istante precedente all'epoca Unix non è rappresentabile qui e restituisce
/// `None`: la campagna lo tratta come errore, non come data plausibile.
pub fn rfc3339(time: SystemTime) -> Option<String> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    let seconds = since.as_secs();
    let millis = since.subsec_millis();
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    ))
}

/// Deadline RFC 3339 a `offset` dall'istante corrente.
pub fn deadline_after(offset: Duration) -> Option<String> {
    rfc3339(SystemTime::now().checked_add(offset)?)
}

/// Giorni dall'epoca Unix in data civile gregoriana (algoritmo di Howard
/// Hinnant, `civil_from_days`), in aritmetica intera esatta.
fn civil_from_days(days: u64) -> Option<(u64, u64, u64)> {
    let z = days.checked_add(719_468)?;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    Some((year, month, day))
}

#[cfg(test)]
mod tests {
    use super::rfc3339;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn known_instants_are_formatted_exactly() {
        assert_eq!(
            rfc3339(UNIX_EPOCH).as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
        // 2000-02-29T12:00:00.250Z: anno bisestile secolare.
        let leap = UNIX_EPOCH + Duration::from_millis(951_825_600_250);
        assert_eq!(rfc3339(leap).as_deref(), Some("2000-02-29T12:00:00.250Z"));
        // 2026-10-05T23:59:59.999Z
        let recent = UNIX_EPOCH + Duration::from_millis(1_791_244_799_999);
        assert_eq!(rfc3339(recent).as_deref(), Some("2026-10-05T23:59:59.999Z"));
    }
}
