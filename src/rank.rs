//! The whole of the ranking policy: fuzzy match quality combined with how
//! often an entry has been launched from here before.
//!
//! GTK-free on purpose, so it is covered by the tests at the bottom of this
//! file.

use crate::apps::Entry;
use crate::matcher;

/// Apply a field's weight to the score it produced.
///
/// Scaling cannot simply multiply: `score` goes negative for a poor match in a
/// long field, and multiplying a negative by a weight below one makes it
/// *larger*, so the comment (0.35) would outrank the keywords (0.45) exactly
/// when both matched badly. Dividing on that side keeps a lower weight worth
/// less whatever the sign.
fn weigh(score: i32, weight: f32) -> i32 {
    let score = score as f32;
    (if score >= 0.0 { score * weight } else { score / weight }) as i32
}

/// Frequently launched apps get a boost, but on a log curve so a good name
/// match on a fresh app can still win.
fn popularity_bonus(count: u32) -> i32 {
    if count == 0 {
        0
    } else {
        (((count + 1) as f32).ln() * 40.0) as i32
    }
}

/// Keep the `lines` best of `ranked` in order and drop the rest. Generic so
/// each caller's comparison inlines — this runs over the whole entry list on
/// the startup path, where a trait object's indirect call blocks that.
fn take_top<F>(ranked: &mut Vec<(usize, i32)>, lines: usize, mut compare: F)
where
    F: FnMut(&(usize, i32), &(usize, i32)) -> std::cmp::Ordering,
{
    // Only `lines` rows are ever displayed, so pull them out with a partial
    // sort and order those. On an empty query that is the difference between
    // ordering six entries and ordering a few thousand.
    if ranked.len() > lines {
        ranked.select_nth_unstable_by(lines, &mut compare);
        ranked.truncate(lines);
    }
    ranked.sort_by(compare);
}

/// Pick the entries to show for `query`, best first, at most `lines` of them.
/// `entries` must be in folded-name order, which is how `Catalog::load`
/// leaves it.
pub fn rank(entries: &[Entry], query: &str, lines: usize) -> Vec<usize> {
    let needle = matcher::Folded::new(query.trim());

    // In both branches below `entries` is in folded-name order, so the index is
    // the alphabetical tie break and no name has to be folded again here. It
    // also makes each comparison a total order, which is what lets `take_top`
    // produce a stable, deterministic list.
    if needle.is_empty() {
        let mut ranked: Vec<(usize, i32)> = (0..entries.len()).map(|i| (i, 0)).collect();
        take_top(&mut ranked, lines, |&(ai, _), &(bi, _)| {
            let (a, b) = (&entries[ai], &entries[bi]);
            b.count
                .cmp(&a.count)
                .then_with(|| b.last_used.cmp(&a.last_used))
                .then_with(|| ai.cmp(&bi))
        });
        return ranked.into_iter().map(|(i, _)| i).collect();
    }

    let mut ranked: Vec<(usize, i32)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            let best = entry
                .fields
                .iter()
                .filter_map(|f| matcher::score(&needle, &f.folded).map(|s| weigh(s, f.weight)))
                .max()?;
            Some((i, best + popularity_bonus(entry.count)))
        })
        .collect();
    take_top(&mut ranked, lines, |&(ai, asc), &(bi, bsc)| {
        let (a, b) = (&entries[ai], &entries[bi]);
        bsc.cmp(&asc)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| ai.cmp(&bi))
    });
    ranked.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::field;

    /// An entry as the ranking sees it: no GIO, no GTK.
    fn entry(name: &str, count: u32, last_used: u64) -> Entry {
        Entry {
            id: format!("{name}.desktop"),
            name: name.to_owned(),
            fields: vec![field(name, 1.0)],
            count,
            last_used,
        }
    }

    /// `Catalog::load` leaves the list in folded-name order and `rank` relies
    /// on it, so the fixtures here do the same.
    fn sorted(mut entries: Vec<Entry>) -> Vec<Entry> {
        entries.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        entries
    }

    fn names(entries: &[Entry], shown: &[usize]) -> Vec<String> {
        shown.iter().map(|&i| entries[i].name.clone()).collect()
    }

    #[test]
    fn a_lower_weight_is_worth_less_whatever_the_sign() {
        // Multiplying would have made the smaller weight the larger number.
        assert!(weigh(-31, 0.45) > weigh(-31, 0.35));
        assert!(weigh(80, 0.45) > weigh(80, 0.35));
        assert_eq!(weigh(0, 0.45), 0);
    }

    #[test]
    fn a_poor_name_match_still_outranks_a_poor_comment_match() {
        // Both score badly enough to go negative, which is where the weighting
        // used to invert and put the description first.
        let mut described = entry("Aardvark Nine", 0, 0);
        described
            .fields
            .push(field("a tool for organising the zebra photographs you keep", 0.35));
        let entries = sorted(vec![described, entry("Zebra", 0, 0)]);
        assert_eq!(names(&entries, &rank(&entries, "zebra", 6))[0], "Zebra");
    }

    #[test]
    fn popularity_is_a_bonus_on_a_log_curve() {
        assert_eq!(popularity_bonus(0), 0);
        assert!(popularity_bonus(1) > 0);
        assert!(popularity_bonus(10) > popularity_bonus(1));
        // Ten times the launches is worth well under ten times the bonus.
        assert!(popularity_bonus(100) < popularity_bonus(10) * 2);
    }

    #[test]
    fn an_empty_query_is_most_used_first() {
        let entries = sorted(vec![
            entry("Alacritty", 0, 0),
            entry("Firefox", 9, 100),
            entry("Gimp", 3, 400),
        ]);
        assert_eq!(names(&entries, &rank(&entries, "", 6)), ["Firefox", "Gimp", "Alacritty"]);
    }

    #[test]
    fn equally_used_entries_fall_back_to_recency_then_name() {
        let entries = sorted(vec![
            entry("Zathura", 2, 500),
            entry("Alacritty", 2, 500),
            entry("Gimp", 2, 900),
        ]);
        assert_eq!(names(&entries, &rank(&entries, "", 6)), ["Gimp", "Alacritty", "Zathura"]);
    }

    #[test]
    fn the_result_list_is_capped_at_lines() {
        let entries = sorted((0..200).map(|i| entry(&format!("app{i:03}"), i, 0)).collect());
        let shown = rank(&entries, "", 6);
        assert_eq!(shown.len(), 6);
        assert_eq!(names(&entries, &shown)[0], "app199");
        assert_eq!(rank(&entries, "app", 4).len(), 4);
    }

    #[test]
    fn a_query_ranks_by_match_quality_not_popularity_alone() {
        let entries = sorted(vec![entry("Firefox", 0, 0), entry("Files", 40, 900)]);
        // "firef" is a much better match than anything in "files", so the
        // popularity bonus must not be able to bury it.
        assert_eq!(names(&entries, &rank(&entries, "firef", 6)), ["Firefox"]);
    }

    #[test]
    fn popularity_breaks_a_tie_between_equal_matches() {
        // Both are a clean prefix match for "term", so nothing but the tie
        // breaks separates them and the shorter name comes first.
        let cold = sorted(vec![entry("Termite", 0, 0), entry("Terminal", 0, 0)]);
        assert_eq!(names(&cold, &rank(&cold, "term", 6)), ["Termite", "Terminal"]);

        // Launch history is enough to turn that around.
        let warm = sorted(vec![entry("Termite", 0, 0), entry("Terminal", 25, 900)]);
        assert_eq!(names(&warm, &rank(&warm, "term", 6)), ["Terminal", "Termite"]);
    }

    #[test]
    fn a_query_that_matches_nothing_shows_nothing() {
        let entries = sorted(vec![entry("Firefox", 5, 0), entry("Gimp", 5, 0)]);
        assert!(rank(&entries, "zzzz", 6).is_empty());
    }

    #[test]
    fn rank_folds_the_query_before_matching() {
        let entries = sorted(vec![entry("Café Player", 0, 0), entry("Gimp", 0, 0)]);
        assert_eq!(names(&entries, &rank(&entries, "  CAFE  ", 6)), ["Café Player"]);
    }
}
