//! Words that are spelled differently but mean the same thing — the way a
//! transcriber writes "vfib" for "v-fib", "lucus" for "LUCAS", "Keystone"
//! for "Keyston". Two cheap tests: edit distance (a letter or two off), and
//! a consonant skeleton (how a word sounds with the vowels worn off).
//!
//! Used by the tripwire preview to suggest what a phrase is really heard
//! as, and meant for street names too.

/// Levenshtein distance, giving up (returning `max + 1`) once it cannot be
/// `max` or less.
pub fn distance(a: &str, b: &str, max: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut best = cur[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            best = best.min(cur[j + 1]);
        }
        if best > max {
            return max + 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()].min(max + 1)
}

/// A rough sound-alike key: lower case, letters only, a few spellings of
/// the same sound folded together, vowels after the first letter dropped,
/// doubled letters collapsed. "Lucas", "lucus" and "lukas" share one.
pub fn skeleton(word: &str) -> String {
    let w: String = word
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    let w = w
        .replace("ph", "f")
        .replace("ck", "k")
        .replace("gh", "g")
        .replace("kn", "n")
        .replace("wr", "r")
        .replace("sch", "sk")
        .replace("tch", "ch");
    let mut out = String::new();
    for (i, c) in w.chars().enumerate() {
        let c = match c {
            'c' | 'q' => 'k',
            'z' => 's',
            'v' => 'f',
            'y' if i > 0 => 'a',
            c => c,
        };
        if i > 0 && "aeiou".contains(c) {
            continue;
        }
        if out.ends_with(c) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Are two single words likely the same word, misheard or misspelled?
pub fn alike(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    if a == b {
        return true;
    }
    let n = a.chars().count().min(b.chars().count());
    // Short words differ by one letter too easily ("cpr", "car"), so they
    // must sound alike; longer ones may be a letter or three off.
    let max = match n {
        0..=3 => 0,
        4..=5 => 1,
        6..=8 => 2,
        _ => 3,
    };
    if max > 0 && distance(&a, &b, max) <= max {
        return true;
    }
    let (sa, sb) = (skeleton(&a), skeleton(&b));
    n >= 3 && sa.len() >= 2 && sa == sb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_edits_and_stops_early() {
        assert_eq!(distance("kitten", "sitting", 5), 3);
        assert_eq!(distance("vfib", "vfib", 2), 0);
        assert_eq!(distance("abc", "abcdefgh", 2), 3, "gave up");
    }

    #[test]
    fn misheard_words_are_alike() {
        assert!(alike("lucas", "lucus"));
        assert!(alike("Keystone", "keyston"));
        assert!(alike("defibrillator", "defibulator"));
        assert!(alike("pulseless", "pulsless"));
        assert!(alike("phone", "fone"));
        assert!(!alike("arrest", "chest"));
        assert!(!alike("cpr", "car"), "short words need to be exact-ish");
        assert!(!alike("medic", "engine"));
    }

    #[test]
    fn skeletons_fold_sounds() {
        assert_eq!(skeleton("Lucas"), skeleton("lukas"));
        assert_eq!(skeleton("Philips"), skeleton("filips"));
        assert_ne!(skeleton("arrest"), skeleton("chest"));
    }
}
