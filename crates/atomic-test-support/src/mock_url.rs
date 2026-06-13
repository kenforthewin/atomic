//! Wiremock-backed mock of a content site — used by the feeds and URL
//! ingestion test paths.
//!
//! Mirrors `MockAiServer`'s shape (a thin handle around `MockServer`) so
//! callers compose the two without juggling unrelated APIs. Serves:
//!
//! - An Atom feed at `GET /feed.xml` with two items linking to
//!   `/article-1` and `/article-2` on the same host.
//! - HTML articles at `GET /article-N` long enough for `readability` to
//!   accept them — minimum prose length is empirically ~300 chars.
//! - Failure-shaped feed variants for the feed-poll ledger tests: a feed
//!   that parses once then 500s ([`MockUrlServer::flaky_feed_url`]), one
//!   that always 500s ([`MockUrlServer::broken_feed_url`]), and one that
//!   responds slowly ([`MockUrlServer::slow_feed_url`]) so tests can
//!   overlap two poll sweeps deterministically.
//!
//! The exact response shapes are tuned to satisfy `feed-rs`'s parser
//! (Atom requires `<id>`, `<updated>`, and at least one `<entry>`) and
//! `readability`'s body-length heuristic (rejects articles below an
//! internal threshold). Tests should treat these as fixed and reuse the
//! helper instead of building feeds inline.

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Response delay on [`MockUrlServer::slow_feed_url`] — long enough that a
/// test driving two poll sweeps concurrently is guaranteed the second
/// sweep's claim attempt lands while the first poll's lease is live.
pub const SLOW_FEED_DELAY: Duration = Duration::from_millis(500);

/// Public re-export of the article HTML so callers can sanity-check the
/// readability heuristic against the same content the mock serves.
pub fn mock_article_html(title: &str) -> String {
    article_html(title)
}

pub struct MockUrlServer {
    server: MockServer,
}

impl MockUrlServer {
    /// Stand up a mock content host with two registered articles and an
    /// Atom feed indexing them. Returns the handle; let it drop to stop
    /// the server. The base URL is suitable for both `POST /api/feeds`
    /// (with `/feed.xml` appended) and `POST /api/ingest` (with
    /// `/article-N` appended).
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let base = server.uri();

        // `set_body_string` forces `Content-Type: text/plain` and overrides
        // any `insert_header` set on the template. Use `set_body_raw` with
        // the explicit content type so the feed parser and the readability
        // gate see the right type on the response.
        Mock::given(method("GET"))
            .and(path("/feed.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                atom_feed(&base, "/feed.xml").into_bytes(),
                "application/atom+xml",
            ))
            .mount(&server)
            .await;

        for (slug, title) in [
            ("/article-1", "Mock Article One"),
            ("/article-2", "Mock Article Two"),
        ] {
            Mock::given(method("GET"))
                .and(path(slug))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(article_html(title).into_bytes(), "text/html; charset=utf-8"),
                )
                .mount(&server)
                .await;
        }

        // A non-HTML path for the "reject non-HTML" ingest contract test.
        Mock::given(method("GET"))
            .and(path("/plaintext"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "just text, nothing structural here".as_bytes().to_vec(),
                "text/plain",
            ))
            .mount(&server)
            .await;

        // A feed that parses exactly once (enough for create-time
        // validation) and 500s on every later fetch — the fixture for poll
        // retry/backoff tests. Mount order matters: wiremock consumes the
        // one-shot success first, then the catch-all 500 takes over.
        Mock::given(method("GET"))
            .and(path("/flaky-feed.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                atom_feed(&base, "/flaky-feed.xml").into_bytes(),
                "application/atom+xml",
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/flaky-feed.xml"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        // A permanently broken feed for tests that seed definitions
        // directly in storage and don't need create-time validation.
        Mock::given(method("GET"))
            .and(path("/broken-feed.xml"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        // A valid feed that responds slowly — see [`SLOW_FEED_DELAY`].
        Mock::given(method("GET"))
            .and(path("/slow-feed.xml"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(SLOW_FEED_DELAY)
                    .set_body_raw(
                        atom_feed(&base, "/slow-feed.xml").into_bytes(),
                        "application/atom+xml",
                    ),
            )
            .mount(&server)
            .await;

        Self { server }
    }

    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    pub fn feed_url(&self) -> String {
        format!("{}/feed.xml", self.server.uri())
    }

    pub fn article_url(&self, n: u32) -> String {
        format!("{}/article-{n}", self.server.uri())
    }

    pub fn plaintext_url(&self) -> String {
        format!("{}/plaintext", self.server.uri())
    }

    /// Feed that serves a valid document on the first fetch and 500s on
    /// every fetch after that. Lets a test create the feed through the
    /// normal validate-on-create path and still observe poll failures.
    pub fn flaky_feed_url(&self) -> String {
        format!("{}/flaky-feed.xml", self.server.uri())
    }

    /// Feed that always responds 500.
    pub fn broken_feed_url(&self) -> String {
        format!("{}/broken-feed.xml", self.server.uri())
    }

    /// Valid feed delayed by [`SLOW_FEED_DELAY`] per response.
    pub fn slow_feed_url(&self) -> String {
        format!("{}/slow-feed.xml", self.server.uri())
    }
}

fn atom_feed(base: &str, self_path: &str) -> String {
    // Atom 1.0 — feed-rs accepts both Atom and RSS, but Atom's `<id>`
    // requirement gives the feed entries stable GUIDs without us having
    // to manage RSS `<guid>` semantics. The `<updated>` field is required
    // by spec but ignored by our parser; we set it to a fixed past date
    // so the test is reproducible. `self_path` keeps each feed variant's
    // `<id>` distinct; the entries (and their article links) are shared —
    // feed-item GUID claims are keyed per feed, so that never collides.
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>{base}{self_path}</id>
  <title>Mock Feed</title>
  <updated>2026-01-01T00:00:00Z</updated>
  <link href="{base}"/>

  <entry>
    <id>{base}/article-1</id>
    <title>Mock Article One</title>
    <link href="{base}/article-1"/>
    <updated>2026-01-01T00:00:00Z</updated>
    <published>2026-01-01T00:00:00Z</published>
    <summary>First mock article.</summary>
  </entry>

  <entry>
    <id>{base}/article-2</id>
    <title>Mock Article Two</title>
    <link href="{base}/article-2"/>
    <updated>2026-01-02T00:00:00Z</updated>
    <published>2026-01-02T00:00:00Z</published>
    <summary>Second mock article.</summary>
  </entry>
</feed>
"#
    )
}

fn article_html(title: &str) -> String {
    // `dom_smoothie`'s `is_probably_readable` plus the post-extraction
    // length floor (200 chars) gate this. The structure mimics a typical
    // long-form article — wrapper `<main>` + `<article>` + multiple
    // paragraphs of substantive prose — so the gate consistently passes.
    // Keep paragraphs distinct so the text-density heuristic doesn't
    // collapse repeats. Empirical: with 3+ paragraphs of ~500 chars
    // around well-formed prose readability scores comfortably above the
    // threshold.
    let paragraphs = [
        "The investigation into the subject began with a careful review of \
         primary sources. Researchers spent several months gathering documents \
         from disparate archives, cross-referencing accounts, and corroborating \
         dates against contemporaneous records held in regional collections.",
        "Once the documentary picture was complete, attention shifted to \
         interviewing surviving witnesses and their descendants. Personal \
         testimony added texture to the official record, frequently revising \
         long-held assumptions about both motivation and chronology.",
        "The final phase involved synthesizing the findings into a single \
         coherent narrative. Editors weighed each claim against the underlying \
         evidence, footnoting where consensus diverged and explaining the \
         reasoning behind each interpretive choice in plain language.",
        "Although the work is now complete, the broader questions it raised \
         remain open. Future researchers will want to revisit several threads \
         that, for reasons of scope or access, this project could not pursue \
         to a satisfying conclusion within the available time.",
    ];
    let body = paragraphs
        .iter()
        .map(|p| format!("    <p>{p}</p>"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <title>{title}</title>
</head>
<body>
  <main>
    <article>
      <h1>{title}</h1>
      <p class="byline">By Mock Author</p>
{body}
    </article>
  </main>
</body>
</html>
"#
    )
}
