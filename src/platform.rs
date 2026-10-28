//! Platform helpers. ProviderKit uses only `std` file APIs,
//! so this module holds the one platform-flavoured conversion
//! the core needs: `SystemTime` to Unix seconds. It works the
//! same on every target and never touches OS-specific APIs.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds between `t` and the Unix epoch. Times before the
/// epoch yield negative values; a time so far out that it does
/// not fit an `i64` yields `None` (reported as JSON null).
pub fn system_time_to_unix(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok(),
        Err(e) => {
            let d = e.duration();
            let secs = i64::try_from(d.as_secs()).ok()?;
            Some(if d.subsec_nanos() > 0 {
                -secs - 1
            } else {
                -secs
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn epoch_and_offsets() {
        assert_eq!(system_time_to_unix(UNIX_EPOCH), Some(0));
        assert_eq!(
            system_time_to_unix(UNIX_EPOCH + Duration::from_secs(90)),
            Some(90)
        );
        assert_eq!(
            system_time_to_unix(UNIX_EPOCH - Duration::from_secs(1)),
            Some(-1)
        );
    }
}
