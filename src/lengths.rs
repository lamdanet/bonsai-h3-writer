//! MiniMax H3 clip lengths: H3 renders 24 fps on a 17k+5 frame grid, so the requested
//! duration is rounded up (6 s -> 158 frames -> 6.58 s). Prompts must use the real length.

/// Durations offered in the UI (seconds).
pub const DURATIONS: [u32; 9] = [3, 4, 5, 6, 8, 10, 12, 15, 20];

/// Frames H3 renders for a requested duration (the ComfyUI math node's formula).
pub fn frames_for_seconds(seconds: f32) -> u32 {
    let n = ((seconds * 24.0).round() as i64).max(5);
    let snapped = n + (5 - n).rem_euclid(17);
    snapped as u32
}

/// Real clip length in seconds for a requested duration.
pub fn real_seconds(seconds: f32) -> f32 {
    frames_for_seconds(seconds) as f32 / 24.0
}

/// Real length with two decimals, as the FL2VA alignment line needs it (`6.58`).
/// Halves round up (10 s = 10.125 s -> `10.13`), matching the guides and system prompts.
pub fn format_real(seconds: f32) -> String {
    let frames = frames_for_seconds(seconds) as u64;
    let hundredths = (frames * 100 * 2 + 24) / 48; // round(frames * 100 / 24), halves up, exact in integers
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// Most cuts that still leave each shot time to play out (~3 s per shot), as in the request
/// templates: 3-4 s -> 1, 6 s -> 2, 10 s -> 3, 15 s -> 4, longer -> about one per 3.5 s.
pub fn suggested_max_cuts(seconds: f32) -> u32 {
    if seconds <= 4.5 {
        1
    } else if seconds <= 6.5 {
        2
    } else if seconds <= 10.5 {
        3
    } else if seconds <= 15.5 {
        4
    } else {
        (seconds / 3.5).floor() as u32
    }
}

/// Largest output size (megapixels) that rendered reliably on the 8 GB laptop.
pub fn laptop_max_megapixels(seconds: f32) -> f32 {
    if seconds <= 6.5 {
        0.5
    } else if seconds <= 10.5 {
        0.4
    } else if seconds <= 15.5 {
        0.3
    } else {
        0.2
    }
}

/// `00:SS.mmm` / `MM:SS.mmm` timestamp used to open `[Shot N]` after the first shot.
pub fn shot_timestamp(seconds: f32) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let minutes = total_ms / 60_000;
    let secs = (total_ms % 60_000) as f64 / 1000.0;
    format!("{minutes:02}:{secs:06.3}")
}

/// Parse `MM:SS.mmm` (or `SS.mmm`) back to seconds.
pub fn parse_timestamp(text: &str) -> Option<f32> {
    let text = text.trim();
    match text.split_once(':') {
        Some((m, s)) => Some(m.trim().parse::<f32>().ok()? * 60.0 + s.trim().parse::<f32>().ok()?),
        None => text.parse::<f32>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_lengths_match_tested_values() {
        for (req, real) in [
            (3.0, "3.04"),
            (4.0, "4.46"),
            (5.0, "5.17"),
            (6.0, "6.58"),
            (8.0, "8.00"),
            (10.0, "10.13"),
            (12.0, "12.25"),
            (15.0, "15.08"),
            (20.0, "20.04"),
        ] {
            assert_eq!(format_real(req), real, "{req} s");
        }
        assert_eq!(frames_for_seconds(6.0), 158);
        assert_eq!((frames_for_seconds(12.0), frames_for_seconds(20.0)), (294, 481));
        assert_eq!(frames_for_seconds(0.0), 5);
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(shot_timestamp(3.5), "00:03.500");
        assert_eq!(shot_timestamp(65.25), "01:05.250");
        assert_eq!(parse_timestamp("00:03.500"), Some(3.5));
        assert_eq!(parse_timestamp("01:05.250"), Some(65.25));
    }
}
