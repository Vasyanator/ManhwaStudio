/*
File: pip_spec.rs

Purpose:
The installer's own reading of the dependency specs it embeds (`utils::BASE_DEPENDENCIES` /
`utils::TORCH_DEPENDENCIES`): the distribution name, the version specifier and the
environment marker of a spec, and whether an installed version (from `pip freeze`)
satisfies that specifier. This is what lets an existing environment be brought to the
pinned versions: a package installed at a version outside its pin counts as missing.

Key structures:
- `Requirement`: one parsed spec (`name`, `specifier`, `marker`).
- `VersionSpecifier`: the comma-separated clauses of a spec (`>=4.57.1,<4.58`).
- `Pep440Version`: a parsed, PEP 440-ordered version.
- `SpecParseError`: why a spec or a version could not be read.

Key functions:
- `Requirement::parse()`, `VersionSpecifier::is_satisfied_by()`, `Pep440Version::parse()`.

Notes:
A deliberate SUBSET of PEP 440 / PEP 508, sized to the embedded lists and to `pip freeze`
output: operators `==`, `!=`, `>=`, `<=`, `>`, `<`, `~=`; versions with epoch, release,
pre (`a` / `b` / `rc`, plus the `alpha` / `beta` / `c` / `pre` / `preview` spellings), post
and dev segments; the local segment (`+cu126`, `+rocm10.1.0`) is ignored for every
comparison, as PEP 440 prescribes for a specifier without a local part. Wildcards
(`==1.2.*`), arbitrary equality (`===`), extras and URL requirements are rejected with a typed
error rather than misread; a unit test in `utils.rs` proves every embedded spec parses. The
marker is returned verbatim: evaluating it stays with
`utils::dependency_marker_matches_current_platform`.
*/

use std::cmp::Ordering;
use std::fmt;

/// Why a dependency spec or an installed version could not be parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SpecParseError {
    /// The spec has no distribution name before its specifier.
    MissingName,
    /// The spec uses a form this subset does not read (extras, URL, `===`, wildcard).
    UnsupportedForm(String),
    /// A specifier clause has no recognised comparison operator.
    UnknownOperator(String),
    /// A version string is not a PEP 440 version.
    InvalidVersion(String),
}

impl fmt::Display for SpecParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => write!(f, "dependency spec has no distribution name"),
            Self::UnsupportedForm(text) => write!(f, "unsupported dependency spec form: '{text}'"),
            Self::UnknownOperator(text) => write!(f, "unknown version operator in clause '{text}'"),
            Self::InvalidVersion(text) => write!(f, "not a PEP 440 version: '{text}'"),
        }
    }
}

/// One parsed dependency spec, borrowing from the spec text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Requirement<'a> {
    /// Distribution name as written (not normalized).
    pub(crate) name: &'a str,
    /// Version clauses; empty when the spec accepts any version.
    pub(crate) specifier: VersionSpecifier,
    /// Environment marker text after `;`, trimmed, verbatim.
    pub(crate) marker: Option<&'a str>,
}

impl<'a> Requirement<'a> {
    /// Parses `name [specifier] [; marker]`.
    ///
    /// # Errors
    /// [`SpecParseError::MissingName`] for an empty name, [`SpecParseError::UnsupportedForm`]
    /// for extras (`[...]`), URL requirements (`@`), `===` or wildcards, and the clause errors
    /// of [`VersionSpecifier::parse`].
    pub(crate) fn parse(spec: &'a str) -> Result<Self, SpecParseError> {
        let (body, marker) = match spec.split_once(';') {
            Some((body, marker)) => (body, Some(marker.trim())),
            None => (spec, None),
        };
        let body = body.trim();
        let name_end = body
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'))
            .unwrap_or(body.len());
        let (name, rest) = body.split_at(name_end);
        if name.is_empty() {
            return Err(SpecParseError::MissingName);
        }
        let rest = rest.trim();
        if rest.starts_with('[') || rest.starts_with('@') {
            return Err(SpecParseError::UnsupportedForm(spec.to_string()));
        }
        Ok(Self { name, specifier: VersionSpecifier::parse(rest)?, marker })
    }
}

/// A comparison operator of one specifier clause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operator {
    Equal,
    NotEqual,
    GreaterOrEqual,
    LessOrEqual,
    Greater,
    Less,
    Compatible,
}

/// One `<operator><version>` clause.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Clause {
    operator: Operator,
    version: Pep440Version,
}

/// The comma-separated version clauses of a spec; a version satisfies it when it satisfies
/// every clause.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct VersionSpecifier {
    clauses: Vec<Clause>,
}

impl VersionSpecifier {
    /// Parses `""`, `"==1.7.2"` or `">=4.57.1, <4.58"`.
    ///
    /// # Errors
    /// [`SpecParseError::UnknownOperator`] for a clause without an operator,
    /// [`SpecParseError::UnsupportedForm`] for `===` or a wildcard version, and
    /// [`SpecParseError::InvalidVersion`] for a clause version that is not PEP 440.
    pub(crate) fn parse(text: &str) -> Result<Self, SpecParseError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(Self::default());
        }
        let clauses = text.split(',').map(parse_clause).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { clauses })
    }

    /// Whether the specifier places no version constraint at all.
    #[must_use]
    pub(crate) fn is_any(&self) -> bool {
        self.clauses.is_empty()
    }

    /// Whether `installed` satisfies every clause.
    ///
    /// An installed pre-release is compared by plain PEP 440 ordering (an already installed
    /// version is accepted like pip accepts it); only the exclusive `<V` / `>V` rules for
    /// pre- and post-releases of `V` itself apply.
    #[must_use]
    pub(crate) fn is_satisfied_by(&self, installed: &Pep440Version) -> bool {
        self.clauses.iter().all(|clause| clause.matches(installed))
    }
}

fn parse_clause(clause: &str) -> Result<Clause, SpecParseError> {
    let clause = clause.trim();
    if clause.starts_with("===") {
        return Err(SpecParseError::UnsupportedForm(clause.to_string()));
    }
    // Two-character operators first, so `>=` is never read as `>` + `=1.0`.
    let (operator, version_text) = [
        ("==", Operator::Equal),
        ("!=", Operator::NotEqual),
        (">=", Operator::GreaterOrEqual),
        ("<=", Operator::LessOrEqual),
        ("~=", Operator::Compatible),
        (">", Operator::Greater),
        ("<", Operator::Less),
    ]
    .iter()
    .find_map(|(prefix, operator)| clause.strip_prefix(prefix).map(|rest| (*operator, rest.trim())))
    .ok_or_else(|| SpecParseError::UnknownOperator(clause.to_string()))?;
    if version_text.ends_with(".*") {
        return Err(SpecParseError::UnsupportedForm(clause.to_string()));
    }
    let version = Pep440Version::parse(version_text)?;
    if operator == Operator::Compatible && version.release.len() < 2 {
        // PEP 440: `~=` needs at least two release segments.
        return Err(SpecParseError::UnsupportedForm(clause.to_string()));
    }
    Ok(Clause { operator, version })
}

impl Clause {
    fn matches(&self, candidate: &Pep440Version) -> bool {
        let spec = &self.version;
        match self.operator {
            Operator::Equal => candidate == spec,
            Operator::NotEqual => candidate != spec,
            Operator::GreaterOrEqual => candidate >= spec,
            Operator::LessOrEqual => candidate <= spec,
            // PEP 440: `<V` excludes pre-releases of V itself unless V is a pre-release, so
            // `<4.58` rejects `4.58.0rc1`.
            Operator::Less => candidate < spec && !(spec.pre.is_none() && candidate.pre.is_some() && candidate.same_release(spec)),
            // PEP 440: `>V` excludes post-releases of V itself unless V is a post-release.
            Operator::Greater => candidate > spec && !(spec.post.is_none() && candidate.post.is_some() && candidate.same_release(spec) && candidate.pre == spec.pre),
            // `~=X.Y.Z` is `>=X.Y.Z, ==X.Y.*`.
            Operator::Compatible => {
                let prefix_len = spec.release.len() - 1;
                candidate >= spec
                    && candidate.epoch == spec.epoch
                    && (0..prefix_len).all(|index| candidate.release_segment(index) == spec.release_segment(index))
            }
        }
    }
}

/// Kind of a pre-release segment, in PEP 440 order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum PreKind {
    Alpha,
    Beta,
    ReleaseCandidate,
}

/// A PEP 440 version without its local segment.
///
/// Equality and ordering follow PEP 440: trailing zero release segments are insignificant
/// (`4.57 == 4.57.0`), `dev < pre < final < post`, and a dev release sorts before the
/// release it develops.
#[derive(Clone, Debug)]
pub(crate) struct Pep440Version {
    epoch: u64,
    release: Vec<u64>,
    pre: Option<(PreKind, u64)>,
    post: Option<u64>,
    dev: Option<u64>,
}

impl Pep440Version {
    /// Parses a version such as `4.57.1`, `2.13.0+rocm10.1.0`, `1!2.0`, `1.0rc1`,
    /// `1.0.post2` or `1.0.dev3`; case-insensitive, one leading `v` allowed.
    ///
    /// # Errors
    /// [`SpecParseError::InvalidVersion`] when the text is not of that shape.
    pub(crate) fn parse(text: &str) -> Result<Self, SpecParseError> {
        let invalid = || SpecParseError::InvalidVersion(text.to_string());
        let lowered = text.trim().to_ascii_lowercase();
        // The local segment never takes part in a comparison against a local-free specifier.
        let public = lowered.split_once('+').map_or(lowered.as_str(), |(public, _)| public);
        let public = public.strip_prefix('v').unwrap_or(public);
        let (epoch, rest) = match public.split_once('!') {
            Some((epoch, rest)) => (epoch.parse::<u64>().map_err(|_| invalid())?, rest),
            None => (0, public),
        };

        let release_end = rest.find(|ch: char| !(ch.is_ascii_digit() || ch == '.')).unwrap_or(rest.len());
        let (release_text, mut suffix) = rest.split_at(release_end);
        // A release ends at its last digit: a trailing `.` belongs to the next segment (`1.0.post1`).
        let release_text = release_text.strip_suffix('.').unwrap_or(release_text);
        if release_text.is_empty() {
            return Err(invalid());
        }
        let release = release_text
            .split('.')
            .map(|part| part.parse::<u64>().map_err(|_| invalid()))
            .collect::<Result<Vec<_>, _>>()?;

        let pre = take_segment(&mut suffix, &[("alpha", PreKind::Alpha), ("a", PreKind::Alpha), ("beta", PreKind::Beta), ("b", PreKind::Beta), ("preview", PreKind::ReleaseCandidate), ("pre", PreKind::ReleaseCandidate), ("rc", PreKind::ReleaseCandidate), ("c", PreKind::ReleaseCandidate)]);
        let post = take_segment(&mut suffix, &[("post", ()), ("rev", ()), ("r", ())]).map(|((), number)| number);
        let dev = take_segment(&mut suffix, &[("dev", ())]).map(|((), number)| number);
        if !suffix.is_empty() {
            return Err(invalid());
        }
        Ok(Self { epoch, release, pre, post, dev })
    }

    /// The release segment at `index`, with the implicit trailing zeros.
    fn release_segment(&self, index: usize) -> u64 {
        self.release.get(index).copied().unwrap_or(0)
    }

    /// Same epoch and release (trailing zeros insignificant).
    fn same_release(&self, other: &Self) -> bool {
        self.epoch == other.epoch && compare_release(&self.release, &other.release) == Ordering::Equal
    }

    /// The PEP 440 sort key of the pre segment: a dev-only release sorts before every
    /// pre-release of the same release, a final release after them.
    fn pre_key(&self) -> (u8, Option<(PreKind, u64)>) {
        match (self.pre, self.post, self.dev) {
            (None, None, Some(_)) => (0, None),
            (Some(pre), _, _) => (1, Some(pre)),
            (None, _, _) => (2, None),
        }
    }
}

/// Consumes one optional `[sep]<label>[sep][number]` segment from the front of `suffix`.
///
/// Separators are `.`, `-` and `_`; a missing number means 0 (PEP 440 normalization).
/// Labels are tried in the given order, so longer spellings must come before their prefixes.
fn take_segment<T: Copy>(suffix: &mut &str, labels: &[(&str, T)]) -> Option<(T, u64)> {
    let trimmed = suffix.trim_start_matches(['.', '-', '_']);
    let (label, rest) = labels.iter().find_map(|(label, value)| trimmed.strip_prefix(label).map(|rest| (*value, rest)))?;
    let rest = rest.trim_start_matches(['.', '-', '_']);
    let digits_end = rest.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(rest.len());
    let (digits, remaining) = rest.split_at(digits_end);
    let number = if digits.is_empty() { 0 } else { digits.parse::<u64>().ok()? };
    *suffix = remaining;
    Some((label, number))
}

/// Orders two release tuples with implicit trailing zeros.
fn compare_release(left: &[u64], right: &[u64]) -> Ordering {
    let len = left.len().max(right.len());
    (0..len)
        .map(|index| left.get(index).copied().unwrap_or(0).cmp(&right.get(index).copied().unwrap_or(0)))
        .find(|ordering| ordering.is_ne())
        .unwrap_or(Ordering::Equal)
}

impl Ord for Pep440Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| compare_release(&self.release, &other.release))
            .then_with(|| self.pre_key().cmp(&other.pre_key()))
            // No post segment sorts before any post-release.
            .then_with(|| self.post.cmp(&other.post))
            // No dev segment sorts AFTER any dev release of the same version.
            .then_with(|| match (self.dev, other.dev) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => left.cmp(&right),
            })
    }
}

impl PartialOrd for Pep440Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Pep440Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Pep440Version {}

#[cfg(test)]
mod tests {
    use super::{Pep440Version, Requirement, SpecParseError, VersionSpecifier};

    fn version(text: &str) -> Pep440Version {
        match Pep440Version::parse(text) {
            Ok(version) => version,
            Err(err) => panic!("test version '{text}' must parse: {err}"),
        }
    }

    fn satisfied(specifier: &str, installed: &str) -> bool {
        match VersionSpecifier::parse(specifier) {
            Ok(specifier) => specifier.is_satisfied_by(&version(installed)),
            Err(err) => panic!("test specifier '{specifier}' must parse: {err}"),
        }
    }

    #[test]
    fn versions_order_by_pep440() {
        let ordered = ["1.0.dev1", "1.0a1", "1.0a2.dev1", "1.0a2", "1.0b1", "1.0rc1", "1.0", "1.0.post1.dev1", "1.0.post1", "1.0.1", "1!0.1"];
        for pair in ordered.windows(2) {
            assert!(version(pair[0]) < version(pair[1]), "{} must sort before {}", pair[0], pair[1]);
        }
        assert_eq!(version("4.57"), version("4.57.0"), "trailing zeros are insignificant");
        assert_eq!(version("2.13.0+rocm10.1.0"), version("2.13.0"), "the local segment is ignored");
        assert_eq!(version("1.0RC1"), version("1.0rc1"));
        assert_eq!(version("1.0-c1"), version("1.0rc1"), "alternate spellings normalize");
        assert_eq!(version("v1.0"), version("1.0"));
    }

    #[test]
    fn invalid_versions_are_errors() {
        for text in ["", "abc", "1.0junk", "1..0", "1!x"] {
            assert!(matches!(Pep440Version::parse(text), Err(SpecParseError::InvalidVersion(_))), "'{text}' must be rejected");
        }
    }

    #[test]
    fn specifier_clauses_combine() {
        assert!(satisfied(">=4.57.1,<4.58", "4.57.1"));
        assert!(satisfied(">=4.57.1,<4.58", "4.57.9"));
        assert!(!satisfied(">=4.57.1,<4.58", "4.57.0"));
        assert!(!satisfied(">=4.57.1,<4.58", "4.58.0"));
        assert!(!satisfied(">=4.57.1,<4.58", "5.20.1"));
        assert!(!satisfied(">=4.57.1,<4.58", "4.58.0rc1"), "`<V` excludes pre-releases of V");
        assert!(satisfied("==1.7.2", "1.7.2"));
        assert!(satisfied("==2.9.1", "2.9.1+cu126"), "a local build satisfies a local-free pin");
        assert!(!satisfied("==1.7.2", "1.7.3"));
        assert!(satisfied("!=1.0", "1.1"));
        assert!(!satisfied("!=1.0", "1.0.0"));
        assert!(satisfied("~=0.39.0", "0.39.5"));
        assert!(!satisfied("~=0.39.0", "0.40.0"));
        assert!(!satisfied(">1.0", "1.0.post1"), "`>V` excludes post-releases of V");
        assert!(satisfied(">1.0", "1.0.1"));
        assert!(satisfied("", "0.0.1"), "an empty specifier accepts any version");
    }

    #[test]
    fn requirements_split_name_specifier_and_marker() {
        let requirement = Requirement::parse("onnxruntime; platform_system != \"Windows\"");
        assert_eq!(requirement.as_ref().map(|req| req.name), Ok("onnxruntime"));
        assert_eq!(requirement.as_ref().map(|req| req.marker), Ok(Some("platform_system != \"Windows\"")));
        assert!(requirement.as_ref().is_ok_and(|req| req.specifier.is_any()));

        let pinned = Requirement::parse("transformers>=4.57.1,<4.58");
        assert_eq!(pinned.as_ref().map(|req| req.name), Ok("transformers"));
        assert!(pinned.as_ref().is_ok_and(|req| !req.specifier.is_any() && req.marker.is_none()));
    }

    #[test]
    fn unsupported_forms_are_rejected() {
        assert_eq!(Requirement::parse(">=1.0"), Err(SpecParseError::MissingName));
        assert!(matches!(Requirement::parse("pkg[extra]>=1"), Err(SpecParseError::UnsupportedForm(_))));
        assert!(matches!(Requirement::parse("pkg @ https://example.invalid/pkg.whl"), Err(SpecParseError::UnsupportedForm(_))));
        assert!(matches!(Requirement::parse("pkg===1.0"), Err(SpecParseError::UnsupportedForm(_))));
        assert!(matches!(Requirement::parse("pkg==1.*"), Err(SpecParseError::UnsupportedForm(_))));
        assert!(matches!(Requirement::parse("pkg~=1"), Err(SpecParseError::UnsupportedForm(_))));
        assert!(matches!(Requirement::parse("pkg 1.0"), Err(SpecParseError::UnknownOperator(_))));
    }
}
