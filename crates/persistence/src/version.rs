//! The order of package versions. A store needs it to tell which installed versions of a package
//! a newly installed one replaces; the installer needs it to choose between versions.

use std::cmp::Ordering;

/// Order two package versions: `major.minor.patch` (missing parts count as 0), then
/// pre-release (`5.3.0-ballot` sorts before `5.3.0` and after `5.2.0`). A version that is not
/// numeric, like `current` or `dev`, means "the newest" and sorts after every numeric one.
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let parse = |text: &str| -> Option<([u64; 3], Option<String>)> {
        let text = text.split('+').next().unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (text, None),
        };
        let mut numbers = [0u64; 3];
        let mut parts = core.split('.');
        for slot in &mut numbers {
            match parts.next() {
                Some(part) => *slot = part.parse().ok()?,
                None => break,
            }
        }
        parts.next().is_none().then_some((numbers, pre))
    };
    match (parse(left), parse(right)) {
        (Some((left_core, left_pre)), Some((right_core, right_pre))) => left_core
            .cmp(&right_core)
            .then_with(|| match (&left_pre, &right_pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_prerelease(a, b),
            }),
        (None, None) => left.cmp(right),
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
    }
}

fn compare_prerelease(left: &str, right: &str) -> Ordering {
    let mut right_parts = right.split('.');
    for a in left.split('.') {
        let Some(b) = right_parts.next() else {
            return Ordering::Greater;
        };
        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => a.cmp(b),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    if right_parts.next().is_some() {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_like_semver() {
        let less = |a: &str, b: &str| compare_versions(a, b) == Ordering::Less;
        assert!(less("5.2.0", "5.3.0-ballot-tc1"));
        assert!(less("5.3.0-ballot-tc1", "5.3.0"));
        assert!(less("6.5.0", "7.1.0"));
        assert!(less("1.0.0-rc.2", "1.0.0-rc.10"));
        assert!(less("1.0.0-alpha", "1.0.0-alpha.1"));
        assert_eq!(compare_versions("4.0.1", "4.0.1"), Ordering::Equal);
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("current", "4.0.1"), Ordering::Greater);
        assert_eq!(compare_versions("4.0.1", "dev"), Ordering::Less);
    }
}
