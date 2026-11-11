//! Version strings used by packages: `3.14.10`, `1.27.4`, `24.22.2`,
//! `8.4.27`, `3.16.0a2`, `27.0.0-alpha.1`.
//!
//! Comparison: the numeric core (up to four dotted numbers) first, then a
//! release sorts after any pre-release of the same core, then pre-release text
//! is compared piecewise (numbers numerically). This orders every upstream
//! scheme the official catalog uses without pretending they are all SemVer.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: Vec<u64>,
    pre: Option<String>,
}

impl Version {
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().trim_start_matches('v');
        if raw.is_empty() || raw.len() > 32 {
            return None;
        }
        // Build metadata (`+…`) never affects ordering.
        let raw = raw.split('+').next().unwrap_or(raw);
        let split = raw
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(raw.len());
        let (core_text, rest) = raw.split_at(split);
        let core_text = core_text.trim_end_matches('.');
        if core_text.is_empty() {
            return None;
        }
        let mut core = Vec::new();
        for part in core_text.split('.') {
            if part.is_empty() || part.len() > 9 {
                return None;
            }
            core.push(part.parse().ok()?);
        }
        if core.len() > 4 {
            return None;
        }
        let pre = rest.trim_start_matches(['-', '.']);
        let pre = if pre.is_empty() {
            None
        } else if pre
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            Some(pre.to_ascii_lowercase())
        } else {
            return None;
        };
        Some(Self { core, pre })
    }

    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }

    /// `major.minor` (or just `major` for one-number versions).
    pub fn default_channel(&self) -> String {
        match self.core.as_slice() {
            [major] => major.to_string(),
            [major, minor, ..] => format!("{major}.{minor}"),
            [] => String::new(),
        }
    }
}

fn component(i: usize, v: &[u64]) -> u64 {
    v.get(i).copied().unwrap_or(0)
}

fn compare_pre(a: &str, b: &str) -> Ordering {
    // Split into alternating digit / non-digit runs and compare run by run.
    fn runs(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, text)) if *d == digit => text.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (ra, rb) = (runs(a), runs(b));
    for (x, y) in ra.iter().zip(rb.iter()) {
        let ord = match (x.0, y.0) {
            (true, true) => {
                let nx: u64 = x.1.parse().unwrap_or(0);
                let ny: u64 = y.1.parse().unwrap_or(0);
                nx.cmp(&ny)
            }
            _ => x.1.cmp(&y.1),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ra.len().cmp(&rb.len())
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.core.len().max(other.core.len());
        for i in 0..len {
            let ord = component(i, &self.core).cmp(&component(i, &other.core));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => compare_pre(a, b),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compare two version strings; unparsable versions sort first.
pub fn compare(a: &str, b: &str) -> Ordering {
    match (Version::parse(a), Version::parse(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

/// `true` when `running` satisfies `minimum` (missing minimum = always).
pub fn satisfies_minimum(running: &str, minimum: Option<&str>) -> bool {
    match minimum {
        None => true,
        Some(min) => compare(running, min) != Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_upstream_schemes() {
        let mut list = vec![
            "3.14.10",
            "3.14.9",
            "3.16.0a2",
            "3.15.2",
            "3.16.0",
            "3.16.0b1",
            "3.16.0rc1",
        ];
        list.sort_by(|a, b| compare(a, b));
        assert_eq!(
            list,
            vec![
                "3.14.9",
                "3.14.10",
                "3.15.2",
                "3.16.0a2",
                "3.16.0b1",
                "3.16.0rc1",
                "3.16.0"
            ]
        );
        assert_eq!(compare("27.0.0-alpha.0", "26.13.0"), Ordering::Greater);
        assert_eq!(compare("27.0.0-alpha.2", "27.0.0-alpha.10"), Ordering::Less);
        assert_eq!(compare("v24.22.2", "24.22.2"), Ordering::Equal);
        assert_eq!(compare("1.27", "1.27.0"), Ordering::Equal);
        assert_eq!(compare("8.4.27+r2", "8.4.27"), Ordering::Equal);
    }

    #[test]
    fn channels_and_minimums() {
        assert_eq!(Version::parse("3.14.10").unwrap().default_channel(), "3.14");
        assert_eq!(Version::parse("24").unwrap().default_channel(), "24");
        assert!(Version::parse("3.16.0a2").unwrap().is_prerelease());
        assert!(satisfies_minimum("0.5.0", Some("0.5.0")));
        assert!(satisfies_minimum("0.10.0", Some("0.5.0")));
        assert!(!satisfies_minimum("0.4.1", Some("0.5.0")));
        assert!(satisfies_minimum("0.4.1", None));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "abc", "1..2", "1.2.3.4.5", "1.2/3", "../1", "1.2 3"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} accepted");
        }
    }
}
