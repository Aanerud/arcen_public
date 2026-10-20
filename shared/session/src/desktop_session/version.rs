//! GNOME's dotted version comparison.

use std::cmp::Ordering;

/// Compares two dotted versions the way GNOME Shell does
/// (`Util.GNOMEversionCompare`): numeric components left to right, a
/// missing component counting as zero; a non-numeric component (`alpha`,
/// `beta`, `rc`) sorts before any number, as a pre-release does.
#[must_use]
pub fn compare(left: &str, right: &str) -> Ordering {
    let mut left = left.trim().split('.');
    let mut right = right.trim().split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (l, r) => {
                let ordering = component(l.unwrap_or("0")).cmp(&component(r.unwrap_or("0")));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

/// Whether `version` is at least `minimum`.
#[must_use]
pub fn at_least(version: &str, minimum: &str) -> bool {
    compare(version, minimum) != Ordering::Less
}

/// A pre-release word sorts below every number; numbers compare by value.
fn component(text: &str) -> (bool, u64) {
    text.trim()
        .parse::<u64>()
        .map_or((false, 0), |number| (true, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_numeric_component() {
        assert_eq!(compare("49.4", "49.4"), Ordering::Equal);
        assert_eq!(compare("49", "49.0"), Ordering::Equal);
        assert_eq!(compare("49.10", "49.9"), Ordering::Greater);
        assert_eq!(compare("40.10", "49.0"), Ordering::Less);
        assert_eq!(compare("50.alpha", "50.0"), Ordering::Less);
        assert_eq!(compare("50.alpha", "49.4"), Ordering::Greater);
        assert!(at_least("47.0", "3.5.91"));
        assert!(!at_least("3.5.90", "3.5.91"));
        assert!(at_least("", ""));
    }
}
