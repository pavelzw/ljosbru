use human_bytes::human_bytes;

pub(crate) fn human_bytes_per_second(bytes_per_second: f64) -> String {
    if bytes_per_second.is_finite() && bytes_per_second > 0.0 {
        format!("{}/s", human_bytes(bytes_per_second))
    } else {
        "0 B/s".to_owned()
    }
}

pub(crate) fn human_eta(remaining_bytes: u64, bytes_per_second: f64) -> String {
    if remaining_bytes == 0 {
        return "0:00".to_owned();
    }

    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return "--:--".to_owned();
    }

    format_duration((remaining_bytes as f64 / bytes_per_second).ceil() as u64)
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_human_bytes_per_second() {
        assert_eq!(human_bytes_per_second(0.0), "0 B/s");
        assert_eq!(human_bytes_per_second(f64::NAN), "0 B/s");
        assert_eq!(human_bytes_per_second(1024.0), "1 KiB/s");
    }

    #[test]
    fn formats_human_eta() {
        assert_eq!(human_eta(0, 0.0), "0:00");
        assert_eq!(human_eta(100, 0.0), "--:--");
        assert_eq!(human_eta(100, f64::NAN), "--:--");
        assert_eq!(human_eta(125, 100.0), "0:02");
        assert_eq!(human_eta(360_000, 100.0), "1:00:00");
    }
}
