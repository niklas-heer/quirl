//! Offline visual index of bounded soak counters and representative VT models.

use super::soak::SoakSummary;
use std::collections::BTreeSet;

/// Render at most 64 screens and feature counters with bounded escaped labels.
/// Links stay relative to the report directory; no scripts or network resources
/// are loaded. Screens describe styled modeled cells, not terminal pixels.
pub(super) fn render(summary: &SoakSummary, labels: &BTreeSet<&'static str>) -> String {
    let mut html = String::from(
        r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src 'self'; style-src 'unsafe-inline'">
<title>Quirl session checks</title>
<style>
:root{color-scheme:dark;font:16px/1.6 system-ui,sans-serif;background:#10141c;color:#e3e8ef}
body{max-width:1280px;margin:0 auto;padding:clamp(20px,4vw,56px)}h1{font-size:clamp(28px,4vw,44px);line-height:1.2;margin-bottom:12px}
h2{margin-top:40px}a{color:#a9ceff;text-underline-offset:3px}p{max-width:85ch;color:#bdc7d5}
.stats,.screens{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,320px),1fr));gap:18px}
.stats{padding:0}.stats div,figure{background:#18202c;border:1px solid #303d50;border-radius:12px;padding:18px;margin:0}
dt{color:#bdc7d5}dd{margin:4px 0 0;font-size:22px;font-variant-numeric:tabular-nums}
figure img{display:block;width:100%;height:auto;background:#10141c}figcaption{margin-bottom:12px;font-weight:600}
ul{padding-left:22px}.notice{border-left:3px solid #b9cbe2;padding-left:18px}footer{margin-top:40px}
</style><body><header><h1>Quirl session checks</h1>
<p>Replayable keyboard journeys and their observed terminal layouts.</p></header>
<nav><a href="summary.json">Run summary</a> · <a href="manifest.json">Replay manifest</a></nav>
"#,
    );
    html.push_str(&format!(
        "<dl class=\"stats\"><div><dt>Actual runtime</dt><dd>{}.{:03} seconds</dd></div><div><dt>Modeled active use</dt><dd>{:.2} hours</dd></div><div><dt>Sessions completed / attempted</dt><dd>{} / {}</dd></div><div><dt>Journeys completed / attempted</dt><dd>{} / {}</dd></div><div><dt>Screen assertions</dt><dd>{}</dd></div><div><dt>Failed sessions</dt><dd>{}</dd></div></dl>\n",
        summary.wall_ms / 1000, summary.wall_ms % 1000, summary.modeled_hours,
        summary.sessions_completed, summary.sessions_attempted, summary.journeys_completed,
        summary.journeys_attempted, summary.screen_assertions, summary.failure_count,
    ));
    html.push_str(&format!("<p>Seed {} · {} key bytes · {} recorded actions · {} resizes.</p><p class=\"notice\">Modeled hours use {} completed journeys per hour with think time removed. They do not establish equivalent real-time endurance. Images show modeled VT cells, SGR colors, styles and cursor using a fixed xterm palette. They do not reproduce terminal themes, font shaping, animated blink or actual terminal pixels.</p>\n", summary.seed, summary.key_bytes, summary.actions, summary.resize_count, summary.journeys_per_modeled_hour));
    if let Some(reason) = &summary.stopped_reason {
        html.push_str(&format!(
            "<p><strong>Run stopped:</strong> {}</p>\n",
            escape(reason)
        ));
    }
    html.push_str("<h2>Completed workflows</h2><ul>\n");
    for (feature, count) in summary.feature_counts.iter().take(64) {
        html.push_str(&format!("<li>{}: {count}</li>\n", escape(feature)));
    }
    if summary.feature_counts.is_empty() {
        html.push_str("<li>No completed workflows recorded.</li>\n");
    }
    html.push_str("</ul><h2>Representative screens</h2><p>Each image is a recorded checkpoint, not a frame from every journey. Open an image to inspect it at its native grid size.</p><div class=\"screens\">\n");
    for label in labels.iter().take(64) {
        let label = escape(label);
        html.push_str(&format!("<figure><figcaption>{label}</figcaption><a href=\"screen-{label}.svg\"><img src=\"screen-{label}.svg\" alt=\"{label}: styled terminal cell model\" loading=\"lazy\"></a></figure>\n"));
    }
    if labels.is_empty() {
        html.push_str("<p>No representative screens were recorded.</p>\n");
    }
    html.push_str("</div><footer><a href=\"summary.json\">Read the full counters and limitations</a></footer></body></html>\n");
    html
}

/// One label retains at most 256 Unicode scalars before HTML expansion.
fn escape(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars().take(256) {
        match character {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&#39;"),
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> SoakSummary {
        SoakSummary {
            schema_version: 1,
            seed: 42,
            sessions_requested: 1,
            sessions_attempted: 1,
            sessions_completed: 1,
            journeys_per_session: 60,
            journeys_attempted: 60,
            journeys_completed: 60,
            key_bytes: 100,
            actions: 20,
            screen_assertions: 10,
            resize_count: 2,
            failure_count: 0,
            failures: vec![],
            wall_ms: 1234,
            modeled_hours: 1.0,
            journeys_per_modeled_hour: 60,
            stopped_reason: None,
            binary_source: "ignored".into(),
            binary_sha256: String::new(),
            binary_bytes: 0,
            report_directory: "ignored".into(),
            feature_counts: Default::default(),
            limitations: "",
        }
    }

    #[test]
    fn gallery_escapes_labels_and_keeps_all_resources_offline() {
        let mut summary = summary();
        summary.stopped_reason = Some("<failure> & 'reason'".to_owned());
        summary.feature_counts.insert("keys<script>".to_owned(), 1);
        let html = render(&summary, &BTreeSet::from(["help\"<&>"]));
        assert!(html.contains("help&quot;&lt;&amp;&gt;"));
        assert!(html.contains("&lt;failure&gt; &amp; &#39;reason&#39;"));
        assert!(html.contains("keys&lt;script&gt;"));
        for forbidden in ["<script", "https://", "http://", "@import", "url("] {
            assert!(!html.contains(forbidden));
        }
        assert!(html.contains("href=\"summary.json\""));
        assert!(html.contains("src=\"screen-help&quot;&lt;&amp;&gt;.svg\""));
        assert!(html.contains("1.234 seconds"));
        assert!(html.contains("1.00 hours"));
    }

    #[test]
    fn empty_gallery_is_normal_and_label_escaping_is_bounded() {
        let html = render(&summary(), &BTreeSet::new());
        assert!(html.contains("No representative screens were recorded"));
        assert!(html.contains("No completed workflows recorded"));
        assert!(html.contains("actual terminal pixels"));
        assert_eq!(escape(&"&".repeat(1000)).len(), 256 * 5);
    }
}
