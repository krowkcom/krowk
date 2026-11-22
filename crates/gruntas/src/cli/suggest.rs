//! "Did you mean": the nearest of the names krowk knows to one it does not,
//! so a typo is answered with the fix rather than only the refusal.

/// The candidate closest to `word` by edit distance, when it is close enough
/// to be the one meant: within a third of the word's length, and at least one
/// edit allowed. A word every candidate is far from gets no guess.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .map(|c| (distance(word, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Levenshtein distance, an adjacent swap counted as one edit.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (rows[i - 1][j] + 1).min(rows[i][j - 1] + 1).min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = d;
        }
    }
    rows[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typo_finds_its_word_and_nonsense_finds_nothing() {
        let names = ["push", "runs", "uploads", "claim", "login"];
        assert_eq!(closest("pussh", names), Some("push"));
        assert_eq!(closest("uplaods", names), Some("uploads"));
        assert_eq!(closest("lgoin", names), Some("login"));
        assert_eq!(closest("frobnicate", names), None);
        assert_eq!(closest("x", names), None);
    }
}
