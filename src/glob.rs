//! Path patterns: matching, and whether two patterns can ever name the same file.
//!
//! Agents declare the files a task is going to touch as glob patterns. Deciding
//! whether two agents are about to collide is therefore not a matching question
//! — no file need exist yet, and the working tree is not the authority anyway —
//! but an *intersection* question: is there any path both patterns describe?
//! [`intersects`] answers it exactly rather than approximately, by walking the
//! product of the two patterns' automata and asking whether an accepting pair
//! is reachable.
//!
//! Syntax, per segment: `*` matches any run of characters inside one segment,
//! `?` matches exactly one, and a whole segment of `**` matches zero or more
//! segments. Everything else is literal. Patterns are `/`-separated, relative
//! to the project root, and case-sensitive.

/// Normalize a user- or agent-supplied pattern into the stored form.
///
/// Leading `./` and `/`, empty segments and `.` segments are dropped, a
/// trailing `/` becomes an explicit `**`, and runs of `**` collapse. `None`
/// means the pattern cannot name anything inside the project — an empty
/// string, or one that climbs out through `..`.
pub fn normalize(pattern: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let trimmed = pattern.trim();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return None,
            // `a/**/**/b` and `a/**/b` describe the same set.
            "**" if out.last() == Some(&"**") => continue,
            other => out.push(other),
        }
    }
    // Checked before the trailing-slash rule below, so a bare `/` stays
    // unusable rather than quietly becoming "the entire project".
    if out.is_empty() {
        return None;
    }
    // A directory, written as such, means everything under it.
    if trimmed.ends_with('/') && out.last() != Some(&"**") {
        out.push("**");
    }
    Some(out.join("/"))
}

/// Does `pattern` describe the whole project?
///
/// Worth surfacing: an agent declaring `**` has told the queue it may touch
/// anything, which conflicts with every other declaration by construction.
pub fn is_everything(pattern: &str) -> bool {
    normalize(pattern).is_some_and(|p| p == "**")
}

/// Does `pattern` name exactly one path, with nothing wild in it?
///
/// The distinction matters wherever hird has to turn intent into files without
/// asking the filesystem: a literal names its one member, while `src/**` names
/// a set whose membership only a directory walk could settle — and guessing at
/// it would be inventing facts.
pub fn is_literal(pattern: &str) -> bool {
    normalize(pattern).is_some_and(|p| !p.contains(['*', '?']))
}

/// Does `path` — a literal path, not a pattern — match `pattern`?
pub fn matches(pattern: &str, path: &str) -> bool {
    let (Some(pattern), Some(path)) = (normalize(pattern), normalize(path)) else {
        return false;
    };
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    path_matches(&pattern, &path)
}

/// Is there any path that both patterns describe?
///
/// Exact: `src/*.rs` and `src/lib*` intersect (`src/lib.rs`), while `src/*.rs`
/// and `src/*.toml` do not, and neither do `a/**/x` and `a/**/y`.
pub fn intersects(a: &str, b: &str) -> bool {
    let (Some(a), Some(b)) = (normalize(a), normalize(b)) else {
        return false;
    };
    let a: Vec<&str> = a.split('/').collect();
    let b: Vec<&str> = b.split('/').collect();
    reachable(&a, &b, segments_agree, |seg| seg == "**")
}

// ------------------------------------------------------------------ matching

fn path_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // Zero or more segments: try every split point. Patterns are a
            // handful of segments deep, so the recursion stays cheap.
            (0..=path.len()).any(|skip| path_matches(rest, &path[skip..]))
        }
        Some((head, rest)) => match path.split_first() {
            Some((first, tail)) if segment_matches(head, first) => path_matches(rest, tail),
            _ => false,
        },
    }
}

/// Classic backtracking glob match within one segment.
fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Where to resume if the current `*` turns out to have eaten too little.
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

// -------------------------------------------------------------- intersection

/// Reachability in the product of two pattern automata.
///
/// States are `(i, j)`, one index into each pattern's symbol list. A `*`-like
/// symbol contributes an epsilon edge (it may match nothing) and a self-loop
/// (it may match one more symbol); everything else advances. The intersection
/// is non-empty exactly when the accepting pair `(len_a, len_b)` is reachable,
/// so this is an emptiness check on an NFA product — no subset construction
/// needed, and no false positives or negatives.
fn reachable<T: Copy>(
    a: &[T],
    b: &[T],
    agree: impl Fn(T, T) -> bool,
    is_star: impl Fn(T) -> bool,
) -> bool {
    let (n, m) = (a.len(), b.len());
    let mut seen = vec![false; (n + 1) * (m + 1)];
    let mut stack = vec![(0usize, 0usize)];

    while let Some((i, j)) = stack.pop() {
        let key = i * (m + 1) + j;
        if std::mem::replace(&mut seen[key], true) {
            continue;
        }
        if i == n && j == m {
            return true;
        }
        // A star on either side may match nothing at all.
        if i < n && is_star(a[i]) {
            stack.push((i + 1, j));
        }
        if j < m && is_star(b[j]) {
            stack.push((i, j + 1));
        }
        if i == n || j == m {
            continue;
        }
        // Both sides consume one symbol, and must agree on what it was.
        if agree(a[i], b[j]) {
            let next_i = if is_star(a[i]) { i } else { i + 1 };
            let next_j = if is_star(b[j]) { j } else { j + 1 };
            stack.push((next_i, next_j));
        }
    }
    false
}

/// Can these two segment patterns describe the same segment?
///
/// `**` agrees with anything: every segment pattern matches at least one
/// non-empty segment, and normalization guarantees no segment is empty.
fn segments_agree(a: &str, b: &str) -> bool {
    if a == "**" || b == "**" {
        return true;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    reachable(&a, &b, chars_agree, |c| c == '*')
}

fn chars_agree(a: char, b: char) -> bool {
    match (a, b) {
        ('*' | '?', _) | (_, '*' | '?') => true,
        (x, y) => x == y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_names_one_path_and_a_glob_names_a_set() {
        assert!(is_literal("src/config.rs"));
        assert!(is_literal("./src/config.rs"));
        assert!(!is_literal("src/*.rs"));
        assert!(!is_literal("src/**"));
        assert!(!is_literal("src/config.r?"));
        // A directory written as one is `**` after normalization, so it is a
        // set even though nothing in it was typed with a star.
        assert!(!is_literal("src/"));
        assert!(!is_literal(""));
    }

    #[test]
    fn normalization_strips_the_noise_a_human_types() {
        assert_eq!(normalize("./src/lib.rs").as_deref(), Some("src/lib.rs"));
        assert_eq!(normalize("/src//lib.rs").as_deref(), Some("src/lib.rs"));
        assert_eq!(normalize("  src/lib.rs  ").as_deref(), Some("src/lib.rs"));
        assert_eq!(normalize("src/./tui").as_deref(), Some("src/tui"));
    }

    #[test]
    fn a_trailing_slash_means_everything_underneath() {
        assert_eq!(normalize("src/").as_deref(), Some("src/**"));
        assert_eq!(normalize("src/**/").as_deref(), Some("src/**"));
        assert_eq!(normalize("/").as_deref(), None);
    }

    #[test]
    fn repeated_globstars_collapse() {
        assert_eq!(normalize("a/**/**/b").as_deref(), Some("a/**/b"));
    }

    #[test]
    fn patterns_that_name_nothing_inside_the_project_are_rejected() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("../secrets"), None);
        assert_eq!(normalize("src/../../etc"), None);
    }

    #[test]
    fn the_whole_project_is_recognisable() {
        assert!(is_everything("**"));
        assert!(is_everything("./**/"));
        assert!(!is_everything("src/**"));
    }

    #[test]
    fn literal_paths_match_themselves() {
        assert!(matches("src/lib.rs", "src/lib.rs"));
        assert!(!matches("src/lib.rs", "src/main.rs"));
        assert!(!matches("src/lib.rs", "src/lib.rs/deeper"));
    }

    #[test]
    fn stars_stay_inside_one_segment() {
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/tui/view.rs"));
        assert!(matches("src/*/view.rs", "src/tui/view.rs"));
    }

    #[test]
    fn question_marks_match_exactly_one_character() {
        assert!(matches("v?.rs", "v1.rs"));
        assert!(!matches("v?.rs", "v10.rs"));
        assert!(!matches("v?.rs", "v.rs"));
    }

    #[test]
    fn globstars_match_zero_or_more_segments() {
        assert!(matches("src/**", "src/lib.rs"));
        assert!(matches("src/**", "src/tui/view.rs"));
        assert!(matches("**/view.rs", "src/tui/view.rs"));
        assert!(matches("**/view.rs", "view.rs"));
        assert!(matches("src/**/view.rs", "src/view.rs"));
        assert!(!matches("src/**", "tests/lib.rs"));
    }

    #[test]
    fn a_directory_pattern_covers_its_contents_but_not_its_sibling() {
        assert!(matches("src/", "src/tui/view.rs"));
        assert!(!matches("src/", "srcery/view.rs"));
    }

    #[test]
    fn overlapping_patterns_are_detected() {
        assert!(intersects("src/lib.rs", "src/lib.rs"));
        assert!(intersects("src/**", "src/tui/view.rs"));
        assert!(intersects("src/*.rs", "src/lib*"));
        assert!(intersects("**", "anything/at/all.rs"));
        assert!(intersects("src/**/mod.rs", "**/mod.rs"));
        assert!(intersects("src/tui/*", "src/*/view.rs"));
    }

    #[test]
    fn disjoint_patterns_are_not() {
        assert!(!intersects("src/lib.rs", "src/main.rs"));
        assert!(!intersects("src/*.rs", "src/*.toml"));
        assert!(!intersects("src/**", "tests/**"));
        assert!(!intersects("src/tui/*", "src/repo/*"));
        // The interesting case: both sides are wildcards, and still disjoint.
        assert!(!intersects("a/**/x.rs", "a/**/y.rs"));
        assert!(!intersects("*.rs", "*.toml"));
    }

    #[test]
    fn intersection_is_symmetric_and_reflexive() {
        let patterns = [
            "src/**",
            "src/*.rs",
            "src/tui/view.rs",
            "**/mod.rs",
            "tests/**",
            "*.toml",
            "**",
        ];
        for a in patterns {
            assert!(intersects(a, a), "{a} must intersect itself");
            for b in patterns {
                assert_eq!(intersects(a, b), intersects(b, a), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn unusable_patterns_never_intersect_or_match() {
        assert!(!intersects("", "**"));
        assert!(!intersects("../x", "**"));
        assert!(!matches("", "src/lib.rs"));
    }

    /// The two answers must agree: if a concrete path matches both patterns,
    /// the patterns intersect. This is the property the conflict radar relies
    /// on, checked over a small exhaustive space rather than by example.
    #[test]
    fn matching_a_common_path_implies_intersection() {
        let patterns = [
            "src/**",
            "src/*.rs",
            "src/lib.rs",
            "src/tui/*.rs",
            "**/view.rs",
            "src/*/view.rs",
            "tests/**",
            "*.toml",
            "**",
            "src/?.rs",
        ];
        let paths = [
            "src/lib.rs",
            "src/main.rs",
            "src/a.rs",
            "src/tui/view.rs",
            "src/tui/app.rs",
            "tests/cli.rs",
            "Cargo.toml",
            "view.rs",
            "docs/design/view.rs",
        ];
        for a in patterns {
            for b in patterns {
                let witness = paths.iter().find(|p| matches(a, p) && matches(b, p));
                if let Some(path) = witness {
                    assert!(
                        intersects(a, b),
                        "{a} and {b} both match {path} but were called disjoint"
                    );
                }
            }
        }
    }

    /// The star-heavy patterns that break naive matchers.
    #[test]
    fn pathological_stars_terminate_with_the_right_answer() {
        assert!(matches("a**a**a**a**b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaab"));
        assert!(!matches("a**a**a**a**b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaac"));
        assert!(intersects("*a*a*a*a*", "*b*b*b*"));
        assert!(!intersects("a*", "b*"));
    }
}
