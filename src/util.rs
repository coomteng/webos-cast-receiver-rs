//! Small shared helpers: injectable clock (ms since epoch) and ISO timestamps for logs.
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. Injectable so tests can freeze time.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    })
}

/// `2026-11-03T10:20:30.123Z`, same shape as Node's `toISOString()`.
pub fn iso_now() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, m, dd) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, dd, sod / 3600, (sod % 3600) / 60, sod % 60, ms
    )
}

// Howard Hinnant's days-from-civil inverse.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// rustls (via reqwest `rustls-no-provider`) needs a crypto provider
/// installed once per process. `ring` keeps the build pure-Rust (no cmake).
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => { println!("{} {}", $crate::util::iso_now(), format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20759), (2026, 11, 2)); // 2026-11-02
    }
}
