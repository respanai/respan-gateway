//! What the formats share to read a model's generation from its id.

/// The version that opens `s`, as `4-5` and `4.5` are 4.5 and `4` is 4.0;
/// a number of three digits or more, such as a date, is no version.
pub(crate) fn version(s: &str) -> Option<(u32, u32)> {
    let (major, rest) = number(s)?;
    let minor = rest.strip_prefix(['-', '.']).and_then(number);
    Some((major, minor.map_or(0, |(minor, _)| minor)))
}

/// The number of one or two digits that opens `s`, and what follows it.
fn number(s: &str) -> Option<(u32, &str)> {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n = s[..digits].parse::<u32>().ok().filter(|_| digits < 3)?;
    Some((n, &s[digits..]))
}
