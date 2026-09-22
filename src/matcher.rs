//! Fuzzy subsequence matcher.
//!
//! Both sides go through [`fold`] first, so the hot path compares plain
//! lowercase ASCII-ish text and never allocates: the caller keeps the folded
//! string and its `Vec<char>` alongside each searchable field, and this module
//! only reads them.

/// Longest haystack, in characters, that gets the optimal placement search.
/// Anything longer falls back to the greedy scan — a name that long is a
/// description, and its exact score does not decide the ordering.
const DP_LIMIT: usize = 96;

/// Text prepared for matching: folded once, with its characters alongside.
///
/// The only way in is [`Folded::new`], so unfolded text cannot reach [`score`]
/// — which is what keeps an accented name searchable without every call site
/// remembering to fold first. Holding both forms is deliberate: `score` needs
/// the `&str` for `starts_with`/`contains` and the `&[char]` for the scan, and
/// building the characters here rather than per call is what keeps the
/// per-keystroke path allocation free.
pub struct Folded {
    pub text: String,
    pub chars: Vec<char>,
}

impl Folded {
    pub fn new(raw: &str) -> Self {
        let text = fold(raw);
        Self { chars: text.chars().collect(), text }
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

/// Lowercase `text` and strip the diacritics off the Latin letters, so `cafe`
/// finds `Café` and `okular` finds `Okular`. Characters outside the folded
/// range are passed through lowercased.
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        for lower in c.to_lowercase() {
            match fold_char(lower) {
                Some(plain) => out.push_str(plain),
                None => out.push(lower),
            }
        }
    }
    out
}

/// Latin-1 Supplement and Latin Extended-A, which is every accented letter a
/// `.desktop` file realistically carries. A few fold to two characters, which
/// is why this hands back a `&str` rather than a `char`.
fn fold_char(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ð' | 'ď' | 'đ' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŋ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' => "s",
        'ß' => "ss",
        'þ' => "th",
        'ţ' | 'ť' | 'ŧ' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// Score `hay` against `needle`. Returns `None` when the needle is not a
/// subsequence of the haystack. Higher is better.
pub fn score(needle: &Folded, hay: &Folded) -> Option<i32> {
    if needle.chars.is_empty() {
        return Some(0);
    }
    // A matched character contributes the same number of bytes on both sides,
    // so fewer bytes in the haystack rules a match out without touching chars.
    if hay.text.len() < needle.text.len() || hay.chars.len() < needle.chars.len() {
        return None;
    }

    // The greedy scan is the exact subsequence test and it is linear, so it
    // runs first and rejects on its own. Only what survives — a small fraction
    // of the list for any query worth typing — pays for the search below, which
    // costs a pass over the haystack per needle character.
    let greedy = greedy_chain(&needle.chars, &hay.chars)?;
    let mut total = if hay.chars.len() <= DP_LIMIT {
        best_chain(&needle.chars, &hay.chars)
    } else {
        greedy
    };

    // Contiguous hits beat scattered ones, short names beat long ones.
    if hay.text.starts_with(&needle.text) {
        total += 60;
    } else if hay.text.contains(&needle.text) {
        total += 40;
    }
    total -= (hay.chars.len() as i32) / 10;

    Some(total)
}

fn boundary(hay: &[char], i: usize) -> bool {
    i == 0 || matches!(hay[i - 1], ' ' | '-' | '_' | '.' | '/' | ':')
}

/// What landing needle character on `hay[i]` is worth by itself.
fn placement(hay: &[char], i: usize) -> i32 {
    if i == 0 {
        50
    } else if boundary(hay, i) {
        40
    } else {
        16
    }
}

/// Everything that depends on where the *first* needle character lands: the
/// run-up that was skipped to reach it, and whether it started a word.
fn opening(hay: &[char], i: usize) -> i32 {
    let mut s = placement(hay, i);
    if !boundary(hay, i) {
        // "co" should find Chromium before it finds Avahi Zeroconf Browser.
        s -= 30;
    }
    s - (i as i32).min(24) * 2
}

/// Best total over every way of placing the needle in the haystack.
///
/// The greedy left-to-right scan this replaced took the first occurrence of
/// each character, which loses a contiguous run further along: `ap` against
/// `a app` would pin `a` at 0 and never find `ap` at 2. One row of the table
/// per needle character, one column per haystack character, so it stays linear
/// in the product and the haystack is capped at [`DP_LIMIT`].
///
/// Only reached once `greedy_chain` has established that the needle *is* a
/// subsequence of the haystack, so some placement always exists.
fn best_chain(needle: &[char], hay: &[char]) -> i32 {
    let width = hay.len();
    let mut prev = [i32::MIN; DP_LIMIT];
    let mut cur = [i32::MIN; DP_LIMIT];

    for (i, slot) in prev[..width].iter_mut().enumerate() {
        if hay[i] == needle[0] {
            *slot = opening(hay, i);
        }
    }

    for &wanted in &needle[1..] {
        cur[..width].fill(i32::MIN);
        // Best predecessor strictly before the previous column, which is the
        // one case that does not earn the adjacency bonus.
        let mut detached = i32::MIN;
        for i in 1..width {
            if i >= 2 {
                detached = detached.max(prev[i - 2]);
            }
            if hay[i] != wanted {
                continue;
            }
            let adjacent = match prev[i - 1] {
                i32::MIN => i32::MIN,
                score => score + 20,
            };
            let from = detached.max(adjacent);
            if from == i32::MIN {
                continue;
            }
            cur[i] = from + placement(hay, i);
        }
        std::mem::swap(&mut prev, &mut cur);
    }

    prev[..width].iter().copied().max().unwrap_or(i32::MIN)
}

/// Take the first occurrence of each needle character and score what that
/// gives. Serves two purposes: it is the exact subsequence test every call
/// runs first, and it is the scoring fallback for haystacks past [`DP_LIMIT`].
fn greedy_chain(needle: &[char], hay: &[char]) -> Option<i32> {
    let mut total = 0i32;
    let mut ni = 0usize;
    let mut prev_match: Option<usize> = None;

    for (i, &c) in hay.iter().enumerate() {
        if ni >= needle.len() {
            break;
        }
        if c != needle[ni] {
            continue;
        }
        total += if ni == 0 {
            opening(hay, i)
        } else if prev_match == Some(i - 1) {
            placement(hay, i) + 20
        } else {
            placement(hay, i)
        };
        prev_match = Some(i);
        ni += 1;
    }

    (ni == needle.len()).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::{fold, score, Folded, DP_LIMIT};

    fn s(needle: &str, hay: &str) -> Option<i32> {
        score(&Folded::new(needle), &Folded::new(hay))
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

    #[test]
    fn a_later_contiguous_run_beats_the_first_occurrence() {
        // The greedy scan pinned "a" at 0 and then had to take a detached "p";
        // the run at index 2 is worth more.
        let best = s("ap", "a app").unwrap();
        let greedy = {
            // 50 for "a" at 0, 16 for a detached "p", +40 for containing "ap",
            // -0 for the length: what the first-occurrence scan would return.
            50 + 16 + 40
        };
        assert!(best > greedy, "{best} !> {greedy}");
    }

    #[test]
    fn long_haystacks_still_match_through_the_fallback() {
        let hay = "z".repeat(DP_LIMIT) + " launchr";
        assert!(s("launchr", &hay).is_some());
        assert!(s("launchrx", &hay).is_none());
    }

    #[test]
    fn folding_strips_diacritics_and_case() {
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(fold("Größe"), "grosse");
        assert_eq!(fold("ŁÓDŹ"), "lodz");
        assert_eq!(fold("Ægir"), "aegir");
    }

    #[test]
    fn an_unaccented_query_finds_an_accented_name() {
        assert!(s("cafe", "Café Player").is_some());
        assert!(s("okular", "Okular").is_some());
    }
}
