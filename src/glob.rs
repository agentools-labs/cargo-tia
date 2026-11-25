//! Minimal glob matching for workspace-relative paths.
//!
//! A pattern matches the whole path, segment by segment:
//!
//! - `*` matches any run of characters inside one segment;
//! - `?` matches one character inside one segment;
//! - a `**` segment matches zero or more whole segments;
//! - a trailing `/` means "everything below this directory" (`docs/` is `docs/**`).
//!
//! Other characters, including `[` and `]`, match themselves.

/// Returns true when `pattern` matches the `/`-separated `path`.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern = match pattern.strip_suffix('/') {
        Some(dir) => format!("{dir}/**"),
        None => pattern.to_string(),
    };
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((segment, path_rest)) => {
                match_segment(first, segment) && match_segments(rest, path_rest)
            }
            None => false,
        },
    }
}

fn match_segment(pattern: &str, segment: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let segment: Vec<char> = segment.chars().collect();
    let (mut p, mut s) = (0, 0);
    // Position of the last `*` in the pattern and the segment position it was tried at.
    let mut star: Option<(usize, usize)> = None;
    while s < segment.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == segment[s]) {
            p += 1;
            s += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, s));
            p += 1;
        } else if let Some((star_p, star_s)) = star {
            p = star_p + 1;
            s = star_s + 1;
            star = Some((star_p, star_s + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn literal_patterns_match_the_whole_path() {
        assert!(matches("Cargo.lock", "Cargo.lock"));
        assert!(!matches("Cargo.lock", "crates/a/Cargo.lock"));
        assert!(!matches("Cargo.lock", "Cargo.lock.bak"));
    }

    #[test]
    fn star_stays_inside_one_segment() {
        assert!(matches("*.md", "README.md"));
        assert!(!matches("*.md", "docs/README.md"));
        assert!(matches("crates/*/build.rs", "crates/a/build.rs"));
        assert!(!matches("crates/*/build.rs", "crates/a/b/build.rs"));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(matches("a*b*c", "abc"));
        assert!(!matches("a*b*c", "aXXbYY"));
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert!(matches("v?.txt", "v1.txt"));
        assert!(matches("v?.txt", "vé.txt"));
        assert!(!matches("v?.txt", "v12.txt"));
        assert!(!matches("a?b", "a/b"));
    }

    #[test]
    fn double_star_matches_any_number_of_segments() {
        assert!(matches("**/build.rs", "build.rs"));
        assert!(matches("**/build.rs", "crates/a/build.rs"));
        assert!(matches("docs/**", "docs/a/b.md"));
        assert!(matches("docs/**", "docs"));
        assert!(matches("a/**/z", "a/z"));
        assert!(matches("a/**/z", "a/b/c/z"));
        assert!(!matches("a/**/z", "b/z"));
        assert!(matches("**", "anything/at/all"));
    }

    #[test]
    fn trailing_slash_means_the_whole_directory() {
        assert!(matches("ci/", "ci/run.sh"));
        assert!(matches("ci/", "ci/deep/run.sh"));
        assert!(!matches("ci/", "src/ci.rs"));
    }

    #[test]
    fn parent_segments_match_literally() {
        assert!(matches("../web/**", "../web/app.ts"));
        assert!(!matches("web/**", "../web/app.ts"));
    }
}
