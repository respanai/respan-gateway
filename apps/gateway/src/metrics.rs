//! Prometheus metrics: `/metrics` answers what the request log has counted
//! since the process started, in the exposition format version 0.0.4, so a
//! scraper reads the gateway's traffic without the admin API.

use crate::app::Process;

/// How a metric family is read over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only ever counted up.
    Counter,
    /// A reading that goes either way.
    Gauge,
}

/// One point of a metric family: its labels and its value.
struct Sample {
    name: String,
    help: String,
    kind: Kind,
    labels: Vec<(&'static str, String)>,
    value: f64,
}

impl Sample {
    /// A point of no labels.
    fn of(name: &str, help: &str, kind: Kind, value: f64) -> Self {
        Self {
            name: name.to_owned(),
            help: help.to_owned(),
            kind,
            labels: Vec::new(),
            value,
        }
    }
}

/// `GET /metrics` answers: the exposition of `process`'s log, each call a
/// provider of `providers` or a tool server of `servers` answered.
pub fn exposition(process: &Process, providers: &[&str], servers: &[&str]) -> String {
    let totals = process.log.totals();
    let mut samples = Vec::new();
    let mut build = Sample::of(
        "respan_gateway_build_info",
        "The gateway's version.",
        Kind::Gauge,
        1.0,
    );
    build.labels.push(("version", crate::VERSION.to_owned()));
    samples.push(build);
    samples.push(Sample::of(
        "respan_gateway_start_time_seconds",
        "When the process started, as seconds since the epoch.",
        Kind::Gauge,
        unix_seconds(process.started_at),
    ));
    samples.push(Sample::of(
        "respan_gateway_uptime_seconds",
        "How long the process has been up.",
        Kind::Gauge,
        process.started.elapsed().as_secs() as f64,
    ));
    samples.push(Sample::of(
        "respan_gateway_requests_total",
        "Requests the gateway has taken, finished or not.",
        Kind::Counter,
        totals.requests as f64,
    ));
    samples.push(Sample::of(
        "respan_gateway_request_errors_total",
        "Requests that ended in error: refused, or a status of 400 or more.",
        Kind::Counter,
        totals.errors as f64,
    ));
    samples.push(Sample::of(
        "respan_gateway_requests_in_flight",
        "Requests open now.",
        Kind::Gauge,
        totals.in_flight as f64,
    ));
    for (direction, tokens) in [
        ("input", totals.input_tokens),
        ("output", totals.output_tokens),
    ] {
        let mut tokens = Sample::of(
            "respan_gateway_tokens_total",
            "Tokens the providers reported.",
            Kind::Counter,
            tokens as f64,
        );
        tokens.labels.push(("direction", direction.to_owned()));
        samples.push(tokens);
    }
    if let Some(cost) = totals.cost_usd {
        samples.push(Sample::of(
            "respan_gateway_cost_usd_total",
            "What the calls cost, in US dollars, by the config's prices.",
            Kind::Counter,
            cost,
        ));
    }
    for name in providers {
        let stats = process.log.provider_stats(name);
        samples.extend(calls("respan_gateway_provider", "provider", name, stats));
    }
    for name in servers {
        let stats = process.log.provider_stats(name);
        samples.extend(calls("respan_gateway_mcp", "server", name, stats));
    }
    render(&samples)
}

/// What a caller answered, as its family's four points.
fn calls(
    prefix: &str,
    label: &'static str,
    name: &str,
    stats: crate::requests::ProviderStats,
) -> Vec<Sample> {
    let mut samples = Vec::new();
    for (suffix, help, value) in [
        (
            "requests_total",
            format!("Calls {name} answered."),
            stats.requests as f64,
        ),
        (
            "errors_total",
            format!("Calls {name} failed."),
            stats.errors as f64,
        ),
        (
            "latency_seconds_sum",
            format!("Time {name}'s calls took, in seconds, to divide by their count."),
            stats.total_ms as f64 / 1_000.0,
        ),
        (
            "latency_seconds_count",
            format!("How many of {name}'s calls their latency sums over."),
            stats.requests as f64,
        ),
    ] {
        let mut sample = Sample::of(&format!("{prefix}_{suffix}"), &help, Kind::Counter, value);
        sample.labels.push((label, name.to_owned()));
        samples.push(sample);
    }
    samples
}

/// `SystemTime` as seconds since the epoch.
fn unix_seconds(at: std::time::SystemTime) -> f64 {
    at.duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The exposition: each family's `# HELP` and `# TYPE` once, then a line
/// per sample, families in the order given.
fn render(samples: &[Sample]) -> String {
    let mut out = String::new();
    let mut named: Option<&str> = None;
    for sample in samples {
        if named != Some(sample.name.as_str()) {
            named = Some(&sample.name);
            out.push_str(&format!(
                "# HELP {} {}\n# TYPE {} {}\n",
                sample.name,
                escape_help(&sample.help),
                sample.name,
                match sample.kind {
                    Kind::Counter => "counter",
                    Kind::Gauge => "gauge",
                }
            ));
        }
        out.push_str(&sample.name);
        if !sample.labels.is_empty() {
            out.push('{');
            for (i, (name, value)) in sample.labels.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(name);
                out.push_str("=\"");
                out.push_str(&escape_label(value));
                out.push('"');
            }
            out.push('}');
        }
        out.push(' ');
        out.push_str(&format_value(sample.value));
        out.push('\n');
    }
    out
}

/// A value without a decimal point when it is whole, as scrapers read either.
fn format_value(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `\`, `"` and a newline, which would end a label's value.
pub fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// `\` and a newline, which would end the line of a `# HELP`.
fn escape_help(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, labels: Vec<(&'static str, &str)>, value: f64) -> Sample {
        let mut sample = Sample::of(name, "help", Kind::Counter, value);
        sample
            .labels
            .extend(labels.into_iter().map(|(k, v)| (k, v.to_owned())));
        sample
    }

    #[test]
    fn a_family_says_what_it_is_once() {
        let samples = [
            sample("m_total", vec![("a", "1")], 2.0),
            sample("m_total", vec![("a", "2")], 3.0),
        ];
        assert_eq!(
            render(&samples),
            "# HELP m_total help\n# TYPE m_total counter\nm_total{a=\"1\"} 2\nm_total{a=\"2\"} 3\n"
        );
    }

    #[test]
    fn families_keep_their_own_help_and_type() {
        let samples = [
            sample("m_total", Vec::new(), 2.0),
            sample("n_total", Vec::new(), 3.0),
            sample("m_total", Vec::new(), 4.0),
        ];
        let shown = render(&samples);
        assert_eq!(shown.matches("# HELP m_total").count(), 2);
        assert_eq!(shown.matches("# HELP n_total").count(), 1);
    }

    #[test]
    fn whole_values_have_no_point_and_the_rest_keep_theirs() {
        assert_eq!(format_value(3.0), "3");
        assert_eq!(format_value(0.5), "0.5");
        assert_eq!(format_value(1_234_567.0), "1234567");
        assert_eq!(format_value(0.0042), "0.0042");
    }

    #[test]
    fn a_label_value_cannot_end_its_line_or_start_another() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(escape_help("a\\b\nc"), "a\\\\b\\nc");
        let samples = [sample("m_total", vec![("p", "a\"b")], 1.0)];
        assert_eq!(
            render(&samples),
            "# HELP m_total help\n# TYPE m_total counter\nm_total{p=\"a\\\"b\"} 1\n"
        );
    }
}
