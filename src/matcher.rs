//! Fuzzy subsequence matcher.
//!
//! Both `needle` and `hay` are expected to be lowercased by the caller so the
//! hot path stays allocation free.

/// Score `hay` against `needle`. Returns `None` when the needle is not a
/// subsequence of the haystack. Higher is better.
pub fn score(needle: &str, needle_chars: &[char], hay: &str) -> Option<i32> {
    if needle_chars.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = hay.chars().collect();
    if h.len() < needle_chars.len() {
        return None;
    }

    let mut total = 0i32;
    let mut ni = 0usize;
    let mut prev_match: Option<usize> = None;
    let mut lead_gap = 0i32;
    let mut first_at_boundary = false;

    for (i, &c) in h.iter().enumerate() {
        if ni >= needle_chars.len() {
            break;
        }
        if c != needle_chars[ni] {
            if ni == 0 {
                lead_gap += 1;
            }
            continue;
        }

        let mut s = 16;
        let boundary = i == 0 || matches!(h[i - 1], ' ' | '-' | '_' | '.' | '/' | ':');
        if i == 0 {
            s += 34;
        } else if boundary {
            s += 24;
        }
        if ni == 0 {
            first_at_boundary = boundary;
        }
        if prev_match == Some(i.wrapping_sub(1)) {
            s += 20;
        }
        total += s;
        prev_match = Some(i);
        ni += 1;
    }

    if ni < needle_chars.len() {
        return None;
    }

    // Contiguous hits beat scattered ones, short names beat long ones.
    if hay.starts_with(needle) {
        total += 60;
    } else if hay.contains(needle) {
        total += 40;
    }
    if !first_at_boundary {
        // "co" should find Chromium before it finds Avahi Zeroconf Browser.
        total -= 30;
    }
    total -= lead_gap.min(24) * 2;
    total -= (h.len() as i32) / 10;

    Some(total)
}

#[cfg(test)]
mod tests {
    use super::score;

    fn s(needle: &str, hay: &str) -> Option<i32> {
        let chars: Vec<char> = needle.chars().collect();
        score(needle, &chars, hay)
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert!(s("zzz", "firefox").is_none());
        assert!(s("firefoxx", "firefox").is_none());
    }

    #[test]
    fn empty_needle_matches_everything() {
        assert_eq!(s("", "anything"), Some(0));
    }

    #[test]
    fn prefix_beats_scattered_match() {
        let prefix = s("fire", "firefox").unwrap();
        let scattered = s("fire", "flatpak internal runtime engine").unwrap();
        assert!(prefix > scattered, "{prefix} !> {scattered}");
    }

    #[test]
    fn word_boundary_beats_mid_word() {
        let boundary = s("vs", "visual studio code").unwrap();
        let mid = s("vs", "aviso serviceon").unwrap();
        assert!(boundary > mid, "{boundary} !> {mid}");
    }

    #[test]
    fn word_start_beats_a_hit_buried_inside_a_word() {
        let word_start = s("co", "chromium").unwrap();
        let buried = s("co", "avahi zeroconf browser").unwrap();
        assert!(word_start > buried, "{word_start} !> {buried}");
    }

    #[test]
    fn shorter_name_wins_on_equal_match() {
        let short = s("term", "terminal").unwrap();
        let long = s("term", "terminal emulator for the gnome desktop").unwrap();
        assert!(short > long, "{short} !> {long}");
    }
}
