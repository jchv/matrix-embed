// This code was authored mostly by Claude Opus 4.6 Thinking.
//
// Yeah, I know, you're tired of hearing that. But this is a low stakes project
// and this is a feature I wanted to support but didn't have the time to work
// on more "properly". Sorry.
//
// I may clean this up more later.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use scraper::Html;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, warn};
use url::Url;

use crate::metadata::Metadata;

/// How long to cache per-host ActivityPub detection results.
const DETECTION_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Timeout for the nodeinfo detection request.
const DETECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for fetching ActivityPub post data.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// ActivityPub content type used in Accept headers.
const AP_CONTENT_TYPE: &str = "application/activity+json";

/// Caches per-host detection of ActivityPub support and provides methods to
/// fetch post data via the ActivityPub protocol.
///
/// Detection is performed by querying `/.well-known/nodeinfo` and verifying
/// that the response contains a valid nodeinfo link.  Results are cached
/// in-memory for [`DETECTION_CACHE_TTL`] so repeated lookups for the same
/// host are essentially free.
pub struct ActivityPubDetector {
    cache: RwLock<HashMap<String, CachedDetection>>,
}

struct CachedDetection {
    supports_activitypub: bool,
    checked_at: Instant,
}

// ---------------------------------------------------------------------------
// Deserialization types for nodeinfo
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct NodeInfoResponse {
    links: Option<Vec<NodeInfoLink>>,
}

#[derive(Deserialize, Debug)]
struct NodeInfoLink {
    rel: Option<String>,
}

// ---------------------------------------------------------------------------
// Deserialization types for ActivityPub objects
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct ActivityPubObject {
    #[serde(rename = "type")]
    object_type: Option<String>,

    /// Content-warning / subject line.
    summary: Option<String>,

    /// HTML body of the post.
    content: Option<String>,

    #[allow(dead_code)]
    sensitive: Option<bool>,

    attachment: Option<Vec<ActivityPubAttachment>>,

    /// Present when the top-level object is an Activity (e.g. `Create`)
    /// wrapping the actual post.
    object: Option<Box<ActivityPubObject>>,

    /// The author of the post — usually a URL string pointing at an Actor,
    /// but can also be an inline Actor object or an array.
    #[serde(rename = "attributedTo")]
    attributed_to: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct ActivityPubAttachment {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,

    /// Can be a plain URL string, a Link object with `href`, or an array of
    /// either.
    url: Option<serde_json::Value>,

    /// Alt-text (unused for now, but deserialized so serde doesn't choke).
    #[allow(dead_code)]
    name: Option<String>,
}

/// Minimal representation of an ActivityPub Actor (`Person`, `Service`, …),
/// used to resolve display name and username for the embed title.
#[derive(Deserialize, Debug, Default)]
struct ActivityPubActor {
    /// Human-readable display name (e.g. "あるるも").
    name: Option<String>,

    /// Local username without the leading `@` (e.g. "arurumo").
    #[serde(rename = "preferredUsername")]
    preferred_username: Option<String>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl ActivityPubDetector {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Check whether `host` advertises ActivityPub support, returning a cached
    /// answer when available.
    pub async fn supports_activitypub(&self, client: &reqwest::Client, host: &str) -> bool {
        // Fast path – read lock only.
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(host) {
                if cached.checked_at.elapsed() < DETECTION_CACHE_TTL {
                    return cached.supports_activitypub;
                }
            }
        }

        // Cache miss or stale – perform the actual check.
        let result = Self::detect_activitypub(client, host).await;
        debug!("ActivityPub detection for {}: {}", host, result);

        {
            let mut cache = self.cache.write().await;
            cache.insert(
                host.to_string(),
                CachedDetection {
                    supports_activitypub: result,
                    checked_at: Instant::now(),
                },
            );
        }

        result
    }

    /// Probe `https://{host}/.well-known/nodeinfo` and return `true` when the
    /// response looks like a valid nodeinfo document.
    async fn detect_activitypub(client: &reqwest::Client, host: &str) -> bool {
        let url = format!("https://{}/.well-known/nodeinfo", host);

        let response = match client.get(&url).timeout(DETECTION_TIMEOUT).send().await {
            Ok(resp) => resp,
            Err(e) => {
                debug!("Nodeinfo request failed for {}: {}", host, e);
                return false;
            }
        };

        if !response.status().is_success() {
            debug!("Nodeinfo returned {} for {}", response.status(), host);
            return false;
        }

        match response.json::<NodeInfoResponse>().await {
            Ok(info) => {
                let valid = info
                    .links
                    .as_ref()
                    .map(|links| {
                        links.iter().any(|l| {
                            l.rel.as_deref().is_some_and(|r| {
                                r.starts_with("http://nodeinfo.diaspora.software/ns/schema/")
                            })
                        })
                    })
                    .unwrap_or(false);
                debug!(
                    "Nodeinfo for {}: {}",
                    host,
                    if valid { "valid" } else { "no matching links" }
                );
                valid
            }
            Err(e) => {
                debug!("Failed to parse nodeinfo JSON for {}: {}", host, e);
                false
            }
        }
    }

    /// Try to obtain [`Metadata`] for `url` via the ActivityPub protocol.
    ///
    /// Returns `Some(Metadata)` when the remote host supports ActivityPub and
    /// the URL resolves to a post-like object (`Note`, `Article`, …).
    /// Returns `None` on any failure, letting the caller fall back to normal
    /// HTML scraping.
    pub async fn fetch_metadata(&self, client: &reqwest::Client, url: &Url) -> Option<Metadata> {
        let host = url.host_str()?;

        if !self.supports_activitypub(client, host).await {
            return None;
        }

        debug!("Fetching ActivityPub data for {}", url);

        let response = match client
            .get(url.clone())
            .timeout(FETCH_TIMEOUT)
            .header(ACCEPT, AP_CONTENT_TYPE)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                debug!("ActivityPub fetch failed for {}: {}", url, e);
                return None;
            }
        };

        if !response.status().is_success() {
            debug!(
                "ActivityPub fetch returned {} for {}",
                response.status(),
                url
            );
            return None;
        }

        // Make sure the server actually gave us JSON, not HTML.
        let ct = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !ct.contains("application/activity+json")
            && !ct.contains("application/ld+json")
            && !ct.contains("application/json")
        {
            debug!(
                "ActivityPub fetch for {} returned unexpected Content-Type: {}",
                url, ct
            );
            return None;
        }

        let obj: ActivityPubObject = match response.json().await {
            Ok(o) => o,
            Err(e) => {
                warn!("Failed to parse ActivityPub JSON for {}: {}", url, e);
                return None;
            }
        };

        // Peek at the note so we can resolve the author before building
        // metadata.  If extraction fails here it will also fail inside
        // ap_object_to_metadata, so returning None is fine.
        let note = extract_note(&obj)?;

        let author_title = self.resolve_author(client, note).await;

        ap_object_to_metadata(&obj, author_title.as_deref())
    }

    /// Try to build an author title string like
    /// `"DisplayName (@username@host)"` from the note's `attributedTo` field.
    async fn resolve_author(
        &self,
        client: &reqwest::Client,
        note: &ActivityPubObject,
    ) -> Option<String> {
        let attributed_to = note.attributed_to.as_ref()?;
        let actor_url = extract_attributed_to_url(attributed_to)?;

        let actor = self.fetch_actor(client, &actor_url).await;
        let host = Url::parse(&actor_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()));

        format_author_title(&actor, host.as_deref())
    }

    /// Fetch an ActivityPub Actor object by URL.
    async fn fetch_actor(&self, client: &reqwest::Client, actor_url: &str) -> ActivityPubActor {
        let response = match client
            .get(actor_url)
            .timeout(FETCH_TIMEOUT)
            .header(ACCEPT, AP_CONTENT_TYPE)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                debug!("Actor fetch returned {} for {}", r.status(), actor_url);
                return ActivityPubActor::default();
            }
            Err(e) => {
                debug!("Actor fetch failed for {}: {}", actor_url, e);
                return ActivityPubActor::default();
            }
        };

        match response.json::<ActivityPubActor>().await {
            Ok(actor) => {
                debug!(
                    "Resolved actor {}: name={:?}, preferredUsername={:?}",
                    actor_url, actor.name, actor.preferred_username
                );
                actor
            }
            Err(e) => {
                debug!("Failed to parse actor JSON from {}: {}", actor_url, e);
                ActivityPubActor::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Unwrap an [`ActivityPubObject`] to reach the inner post-like object,
/// handling both bare Notes and wrapping Activities (`Create`, `Announce`).
fn extract_note(obj: &ActivityPubObject) -> Option<&ActivityPubObject> {
    let note = match obj.object_type.as_deref() {
        Some("Create") | Some("Announce") => obj.object.as_deref()?,
        Some("Note") | Some("Article") | Some("Question") | Some("Page") => obj,
        Some(other) => {
            debug!("Ignoring ActivityPub object of type '{}'", other);
            return None;
        }
        None => return None,
    };

    match note.object_type.as_deref() {
        Some("Note") | Some("Article") | Some("Question") | Some("Page") => Some(note),
        _ => None,
    }
}

/// Extract an actor URL from an `attributedTo` value, which can be a plain
/// URL string, an object with an `id` field, or an array of either.
fn extract_attributed_to_url(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(obj) => obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        serde_json::Value::Array(arr) => arr.first().and_then(extract_attributed_to_url),
        _ => None,
    }
}

/// Format an author title string like `"DisplayName (@username@host)"` from
/// actor profile fields, matching the format typically used by OpenGraph
/// metadata on Fediverse instances.
fn format_author_title(actor: &ActivityPubActor, host: Option<&str>) -> Option<String> {
    let name = actor.name.as_deref().filter(|s| !s.is_empty());
    let username = actor
        .preferred_username
        .as_deref()
        .filter(|s| !s.is_empty());

    match (name, username, host) {
        (Some(name), Some(user), Some(host)) => Some(format!("{} (@{}@{})", name, user, host)),
        (Some(name), Some(user), None) => Some(format!("{} (@{})", name, user)),
        (Some(name), None, _) => Some(name.to_string()),
        (None, Some(user), Some(host)) => Some(format!("@{}@{}", user, host)),
        (None, Some(user), None) => Some(format!("@{}", user)),
        (None, None, _) => None,
    }
}

/// Turn an [`ActivityPubObject`] into [`Metadata`], handling both bare `Note`
/// objects and `Create` / `Announce` activities that wrap one.
///
/// When `author_title` is provided it is used as the metadata title (matching
/// the usual OpenGraph behaviour) and any content-warning / summary text is
/// folded into the description.  When `author_title` is `None` the summary is
/// used as the title instead (fallback).
fn ap_object_to_metadata(obj: &ActivityPubObject, author_title: Option<&str>) -> Option<Metadata> {
    let note = extract_note(obj)?;

    let cw = note.summary.as_deref().filter(|s| !s.is_empty());
    let content = note.content.as_deref().map(strip_html);
    let content = content.as_deref().filter(|s| !s.is_empty());

    // When we have author information use it as the title (matching the
    // format OG tags normally provide, e.g. "あるるも (@arurumo@misskey.io)").
    // The CW, if present, is prepended to the description.
    // When author resolution failed, fall back to the CW as the title.
    let title;
    let description;

    if let Some(author) = author_title {
        title = Some(author.to_string());
        description = match (cw, content) {
            (Some(cw), Some(text)) => Some(format!("{}\n{}", cw, text)),
            (Some(cw), None) => Some(cw.to_string()),
            (None, Some(text)) => Some(text.to_string()),
            (None, None) => None,
        };
    } else {
        title = cw.map(|s| s.to_string());
        description = content.map(|s| s.to_string());
    }

    let mut image_url: Option<Url> = None;
    let mut video_url: Option<Url> = None;
    let mut audio_url: Option<Url> = None;

    if let Some(attachments) = &note.attachment {
        for att in attachments {
            let media_type = att.media_type.as_deref().unwrap_or("");

            let Some(raw_url) = extract_attachment_url(att) else {
                continue;
            };
            let Ok(parsed) = Url::parse(&raw_url) else {
                continue;
            };

            if media_type.starts_with("image/") && image_url.is_none() {
                image_url = Some(parsed);
            } else if media_type.starts_with("video/") && video_url.is_none() {
                video_url = Some(parsed);
            } else if media_type.starts_with("audio/") && audio_url.is_none() {
                audio_url = Some(parsed);
            }
        }
    }

    let metadata = Metadata {
        card: None,
        title,
        description,
        image_url,
        video_url,
        audio_url,
    };

    if metadata.is_empty() {
        return None;
    }

    Some(metadata)
}

/// Extract the URL string from an attachment's `url` field, which may be a
/// plain string, a `Link` object with an `href` key, or an array of either.
fn extract_attachment_url(att: &ActivityPubAttachment) -> Option<String> {
    match &att.url {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Object(obj)) => obj
            .get("href")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Some(serde_json::Value::Array(arr)) => arr.first().and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(obj) => obj
                .get("href")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }),
        _ => None,
    }
}

/// Convert HTML to plain text, roughly preserving line breaks from `<br>` and
/// `</p>` tags.
fn strip_html(html: &str) -> String {
    let prepared = html
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</p>", "\n")
        .replace("</li>", "\n");

    let fragment = Html::parse_fragment(&prepared);
    let raw: String = fragment.root_element().text().collect::<Vec<_>>().join("");

    // Collapse runs of blank lines while keeping intentional line breaks.
    let mut result = String::new();
    let mut prev_blank = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank && !result.is_empty() {
                result.push('\n');
                prev_blank = true;
            }
        } else {
            if !result.is_empty() && !prev_blank {
                result.push('\n');
            }
            result.push_str(trimmed);
            prev_blank = false;
        }
    }

    result.trim().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- strip_html ---------------------------------------------------------

    #[test]
    fn test_strip_html_plain_text() {
        assert_eq!(strip_html("hello"), "hello");
    }

    #[test]
    fn test_strip_html_basic_paragraph() {
        assert_eq!(strip_html("<p>Hello world</p>"), "Hello world");
    }

    #[test]
    fn test_strip_html_with_links() {
        assert_eq!(
            strip_html(r#"<p>Check <a href="https://example.com">this</a> out</p>"#),
            "Check this out"
        );
    }

    #[test]
    fn test_strip_html_br_tags() {
        assert_eq!(strip_html("<p>Line 1<br>Line 2</p>"), "Line 1\nLine 2");
        assert_eq!(strip_html("<p>A<br/>B<br />C</p>"), "A\nB\nC");
    }

    #[test]
    fn test_strip_html_multiple_paragraphs() {
        let result = strip_html("<p>First</p><p>Second</p>");
        assert!(result.contains("First"));
        assert!(result.contains("Second"));
    }

    #[test]
    fn test_strip_html_empty() {
        assert_eq!(strip_html(""), "");
    }

    #[test]
    fn test_strip_html_only_tags() {
        assert_eq!(strip_html("<p></p><br><p></p>"), "");
    }

    #[test]
    fn test_strip_html_entities() {
        assert_eq!(strip_html("<p>a &amp; b &lt; c</p>"), "a & b < c");
    }

    // -- extract_attachment_url ---------------------------------------------

    #[test]
    fn test_extract_attachment_url_string() {
        let att = ActivityPubAttachment {
            media_type: Some("image/jpeg".into()),
            url: Some(serde_json::Value::String(
                "https://example.com/img.jpg".into(),
            )),
            name: None,
        };
        assert_eq!(
            extract_attachment_url(&att),
            Some("https://example.com/img.jpg".into())
        );
    }

    #[test]
    fn test_extract_attachment_url_object() {
        let att = ActivityPubAttachment {
            media_type: Some("image/jpeg".into()),
            url: Some(serde_json::json!({
                "type": "Link",
                "href": "https://example.com/img.jpg"
            })),
            name: None,
        };
        assert_eq!(
            extract_attachment_url(&att),
            Some("https://example.com/img.jpg".into())
        );
    }

    #[test]
    fn test_extract_attachment_url_array_of_objects() {
        let att = ActivityPubAttachment {
            media_type: Some("image/jpeg".into()),
            url: Some(serde_json::json!([
                { "type": "Link", "href": "https://example.com/a.jpg" },
                { "type": "Link", "href": "https://example.com/b.jpg" }
            ])),
            name: None,
        };
        assert_eq!(
            extract_attachment_url(&att),
            Some("https://example.com/a.jpg".into())
        );
    }

    #[test]
    fn test_extract_attachment_url_array_of_strings() {
        let att = ActivityPubAttachment {
            media_type: Some("image/jpeg".into()),
            url: Some(serde_json::json!(["https://example.com/a.jpg"])),
            name: None,
        };
        assert_eq!(
            extract_attachment_url(&att),
            Some("https://example.com/a.jpg".into())
        );
    }

    #[test]
    fn test_extract_attachment_url_none() {
        let att = ActivityPubAttachment {
            media_type: Some("image/jpeg".into()),
            url: None,
            name: None,
        };
        assert_eq!(extract_attachment_url(&att), None);
    }

    #[test]
    fn test_extract_attachment_url_null() {
        let att = ActivityPubAttachment {
            media_type: Some("image/jpeg".into()),
            url: Some(serde_json::Value::Null),
            name: None,
        };
        assert_eq!(extract_attachment_url(&att), None);
    }

    // -- ap_object_to_metadata ----------------------------------------------

    #[test]
    fn test_note_with_image() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: Some("CW: spoilers".into()),
            content: Some("<p>Here is the post</p>".into()),
            sensitive: Some(true),
            attachment: Some(vec![ActivityPubAttachment {
                media_type: Some("image/jpeg".into()),
                url: Some(serde_json::Value::String(
                    "https://files.example.com/photo.jpg".into(),
                )),
                name: Some("Alt text".into()),
            }]),
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        assert_eq!(meta.title.as_deref(), Some("CW: spoilers"));
        assert_eq!(meta.description.as_deref(), Some("Here is the post"));
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://files.example.com/photo.jpg").unwrap())
        );
        assert!(meta.video_url.is_none());
        assert!(meta.audio_url.is_none());
    }

    #[test]
    fn test_create_wrapping_note() {
        let obj = ActivityPubObject {
            object_type: Some("Create".into()),
            summary: None,
            content: None,
            sensitive: None,
            attachment: None,
            object: Some(Box::new(ActivityPubObject {
                object_type: Some("Note".into()),
                summary: None,
                content: Some("<p>Inner note</p>".into()),
                sensitive: Some(false),
                attachment: Some(vec![ActivityPubAttachment {
                    media_type: Some("image/png".into()),
                    url: Some(serde_json::Value::String(
                        "https://cdn.example.com/pic.png".into(),
                    )),
                    name: None,
                }]),
                object: None,
                attributed_to: None,
            })),
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        assert!(meta.title.is_none());
        assert_eq!(meta.description.as_deref(), Some("Inner note"));
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://cdn.example.com/pic.png").unwrap())
        );
    }

    #[test]
    fn test_note_no_summary() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: None,
            content: Some("<p>Just text, no CW</p>".into()),
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        assert!(meta.title.is_none());
        assert_eq!(meta.description.as_deref(), Some("Just text, no CW"));
    }

    #[test]
    fn test_note_empty_summary_treated_as_none() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: Some("".into()),
            content: Some("<p>Content here</p>".into()),
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        assert!(meta.title.is_none());
    }

    #[test]
    fn test_person_object_ignored() {
        let obj = ActivityPubObject {
            object_type: Some("Person".into()),
            summary: Some("I'm a person".into()),
            content: None,
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        assert!(ap_object_to_metadata(&obj, None).is_none());
    }

    #[test]
    fn test_empty_note_returns_none() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: None,
            content: None,
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        assert!(ap_object_to_metadata(&obj, None).is_none());
    }

    #[test]
    fn test_no_type_returns_none() {
        let obj = ActivityPubObject {
            object_type: None,
            summary: None,
            content: Some("<p>mystery</p>".into()),
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        assert!(ap_object_to_metadata(&obj, None).is_none());
    }

    #[test]
    fn test_multiple_attachment_types() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: None,
            content: Some("<p>Media post</p>".into()),
            sensitive: None,
            attachment: Some(vec![
                ActivityPubAttachment {
                    media_type: Some("image/jpeg".into()),
                    url: Some(serde_json::Value::String(
                        "https://cdn.example.com/a.jpg".into(),
                    )),
                    name: None,
                },
                ActivityPubAttachment {
                    media_type: Some("image/png".into()),
                    url: Some(serde_json::Value::String(
                        "https://cdn.example.com/b.png".into(),
                    )),
                    name: None,
                },
                ActivityPubAttachment {
                    media_type: Some("video/mp4".into()),
                    url: Some(serde_json::Value::String(
                        "https://cdn.example.com/v.mp4".into(),
                    )),
                    name: None,
                },
                ActivityPubAttachment {
                    media_type: Some("audio/mpeg".into()),
                    url: Some(serde_json::Value::String(
                        "https://cdn.example.com/a.mp3".into(),
                    )),
                    name: None,
                },
            ]),
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        // First of each type wins.
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://cdn.example.com/a.jpg").unwrap())
        );
        assert_eq!(
            meta.video_url,
            Some(Url::parse("https://cdn.example.com/v.mp4").unwrap())
        );
        assert_eq!(
            meta.audio_url,
            Some(Url::parse("https://cdn.example.com/a.mp3").unwrap())
        );
    }

    #[test]
    fn test_attachment_with_bad_url_skipped() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: None,
            content: Some("<p>Post</p>".into()),
            sensitive: None,
            attachment: Some(vec![
                ActivityPubAttachment {
                    media_type: Some("image/jpeg".into()),
                    url: Some(serde_json::Value::String("not a url".into())),
                    name: None,
                },
                ActivityPubAttachment {
                    media_type: Some("image/jpeg".into()),
                    url: Some(serde_json::Value::String(
                        "https://cdn.example.com/good.jpg".into(),
                    )),
                    name: None,
                },
            ]),
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://cdn.example.com/good.jpg").unwrap())
        );
    }

    #[test]
    fn test_article_type_supported() {
        let obj = ActivityPubObject {
            object_type: Some("Article".into()),
            summary: Some("My Blog Post".into()),
            content: Some("<p>Long form content here.</p>".into()),
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        assert_eq!(meta.title.as_deref(), Some("My Blog Post"));
        assert_eq!(meta.description.as_deref(), Some("Long form content here."));
    }

    #[test]
    fn test_attachment_with_no_media_type_skipped() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: None,
            content: Some("<p>hello</p>".into()),
            sensitive: None,
            attachment: Some(vec![ActivityPubAttachment {
                media_type: None,
                url: Some(serde_json::Value::String(
                    "https://example.com/mystery".into(),
                )),
                name: None,
            }]),
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, None).unwrap();
        // Unknown media type doesn't match image/video/audio, so no media URL.
        assert!(meta.image_url.is_none());
        assert!(meta.video_url.is_none());
        assert!(meta.audio_url.is_none());
        // But we still get the text content.
        assert_eq!(meta.description.as_deref(), Some("hello"));
    }

    // -- integration-style: round-trip from JSON ----------------------------

    #[test]
    fn test_mastodon_style_note() {
        let json = serde_json::json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/v1"
            ],
            "id": "https://mastodon.social/users/alice/statuses/123",
            "type": "Note",
            "summary": null,
            "content": "<p>Look at this cool photo!</p>",
            "sensitive": true,
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "image/webp",
                    "url": "https://files.mastodon.social/media/original/abc123.webp",
                    "name": "A cool photo",
                    "blurhash": "LEHV6nWB2y"
                }
            ],
            "attributedTo": "https://mastodon.social/users/alice",
            "published": "2025-01-15T12:00:00Z"
        });

        let obj: ActivityPubObject = serde_json::from_value(json).unwrap();
        let meta = ap_object_to_metadata(&obj, None).unwrap();

        assert!(meta.title.is_none()); // summary was null
        assert_eq!(
            meta.description.as_deref(),
            Some("Look at this cool photo!")
        );
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://files.mastodon.social/media/original/abc123.webp").unwrap())
        );
    }

    #[test]
    fn test_misskey_style_note() {
        let json = serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://misskey.io/notes/abcdef",
            "type": "Note",
            "summary": "CW: NSFW",
            "content": "<p><span>sensitive art post</span></p>",
            "sensitive": true,
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "image/webp",
                    "url": "https://media.misskeycdn.com/abc.webp",
                    "name": null,
                    "sensitive": true
                }
            ]
        });

        let obj: ActivityPubObject = serde_json::from_value(json).unwrap();
        let meta = ap_object_to_metadata(&obj, None).unwrap();

        assert_eq!(meta.title.as_deref(), Some("CW: NSFW"));
        assert_eq!(meta.description.as_deref(), Some("sensitive art post"));
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://media.misskeycdn.com/abc.webp").unwrap())
        );
    }

    // -- format_author_title ------------------------------------------------

    #[test]
    fn test_format_author_title_full() {
        let actor = ActivityPubActor {
            name: Some("あるるも".into()),
            preferred_username: Some("arurumo".into()),
        };
        assert_eq!(
            format_author_title(&actor, Some("misskey.io")),
            Some("あるるも (@arurumo@misskey.io)".into())
        );
    }

    #[test]
    fn test_format_author_title_no_host() {
        let actor = ActivityPubActor {
            name: Some("Alice".into()),
            preferred_username: Some("alice".into()),
        };
        assert_eq!(
            format_author_title(&actor, None),
            Some("Alice (@alice)".into())
        );
    }

    #[test]
    fn test_format_author_title_name_only() {
        let actor = ActivityPubActor {
            name: Some("Alice".into()),
            preferred_username: None,
        };
        assert_eq!(
            format_author_title(&actor, Some("example.com")),
            Some("Alice".into())
        );
    }

    #[test]
    fn test_format_author_title_username_only() {
        let actor = ActivityPubActor {
            name: None,
            preferred_username: Some("alice".into()),
        };
        assert_eq!(
            format_author_title(&actor, Some("example.com")),
            Some("@alice@example.com".into())
        );
    }

    #[test]
    fn test_format_author_title_username_no_host() {
        let actor = ActivityPubActor {
            name: None,
            preferred_username: Some("alice".into()),
        };
        assert_eq!(format_author_title(&actor, None), Some("@alice".into()));
    }

    #[test]
    fn test_format_author_title_empty() {
        let actor = ActivityPubActor {
            name: None,
            preferred_username: None,
        };
        assert_eq!(format_author_title(&actor, Some("example.com")), None);
    }

    #[test]
    fn test_format_author_title_blank_name_falls_back() {
        let actor = ActivityPubActor {
            name: Some("".into()),
            preferred_username: Some("alice".into()),
        };
        assert_eq!(
            format_author_title(&actor, Some("example.com")),
            Some("@alice@example.com".into())
        );
    }

    // -- extract_attributed_to_url ------------------------------------------

    #[test]
    fn test_extract_attributed_to_url_string() {
        let val = serde_json::json!("https://mastodon.social/users/alice");
        assert_eq!(
            extract_attributed_to_url(&val),
            Some("https://mastodon.social/users/alice".into())
        );
    }

    #[test]
    fn test_extract_attributed_to_url_object() {
        let val = serde_json::json!({
            "type": "Person",
            "id": "https://mastodon.social/users/alice",
            "name": "Alice"
        });
        assert_eq!(
            extract_attributed_to_url(&val),
            Some("https://mastodon.social/users/alice".into())
        );
    }

    #[test]
    fn test_extract_attributed_to_url_array() {
        let val = serde_json::json!([
            "https://mastodon.social/users/alice",
            "https://example.com/bob"
        ]);
        assert_eq!(
            extract_attributed_to_url(&val),
            Some("https://mastodon.social/users/alice".into())
        );
    }

    #[test]
    fn test_extract_attributed_to_url_null() {
        assert_eq!(extract_attributed_to_url(&serde_json::Value::Null), None);
    }

    // -- author-aware metadata conversion -----------------------------------

    #[test]
    fn test_note_with_author_and_cw() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: Some("CW: spoilers".into()),
            content: Some("<p>The actual post</p>".into()),
            sensitive: Some(true),
            attachment: Some(vec![ActivityPubAttachment {
                media_type: Some("image/jpeg".into()),
                url: Some(serde_json::Value::String(
                    "https://files.example.com/photo.jpg".into(),
                )),
                name: None,
            }]),
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, Some("Alice (@alice@example.com)")).unwrap();
        // Author becomes the title.
        assert_eq!(meta.title.as_deref(), Some("Alice (@alice@example.com)"));
        // CW is folded into the description before the content.
        assert_eq!(
            meta.description.as_deref(),
            Some("CW: spoilers\nThe actual post")
        );
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://files.example.com/photo.jpg").unwrap())
        );
    }

    #[test]
    fn test_note_with_author_no_cw() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: None,
            content: Some("<p>Just a normal post</p>".into()),
            sensitive: None,
            attachment: None,
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, Some("Bob (@bob@example.com)")).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Bob (@bob@example.com)"));
        assert_eq!(meta.description.as_deref(), Some("Just a normal post"));
    }

    #[test]
    fn test_note_with_author_cw_only() {
        let obj = ActivityPubObject {
            object_type: Some("Note".into()),
            summary: Some("CW: secret".into()),
            content: None,
            sensitive: Some(true),
            attachment: None,
            object: None,
            attributed_to: None,
        };

        let meta = ap_object_to_metadata(&obj, Some("Carol (@carol@example.com)")).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Carol (@carol@example.com)"));
        // CW is the only text, so it becomes the description.
        assert_eq!(meta.description.as_deref(), Some("CW: secret"));
    }

    #[test]
    fn test_mastodon_style_note_with_author() {
        let json = serde_json::json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/v1"
            ],
            "id": "https://mastodon.social/users/alice/statuses/123",
            "type": "Note",
            "summary": null,
            "content": "<p>Look at this cool photo!</p>",
            "sensitive": true,
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "image/webp",
                    "url": "https://files.mastodon.social/media/original/abc123.webp",
                    "name": "A cool photo",
                    "blurhash": "LEHV6nWB2y"
                }
            ],
            "attributedTo": "https://mastodon.social/users/alice",
            "published": "2025-01-15T12:00:00Z"
        });

        let obj: ActivityPubObject = serde_json::from_value(json).unwrap();
        let meta = ap_object_to_metadata(&obj, Some("Alice (@alice@mastodon.social)")).unwrap();

        assert_eq!(
            meta.title.as_deref(),
            Some("Alice (@alice@mastodon.social)")
        );
        assert_eq!(
            meta.description.as_deref(),
            Some("Look at this cool photo!")
        );
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://files.mastodon.social/media/original/abc123.webp").unwrap())
        );
    }

    #[test]
    fn test_misskey_style_note_with_author() {
        let json = serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://misskey.io/notes/abcdef",
            "type": "Note",
            "summary": "CW: NSFW",
            "content": "<p><span>sensitive art post</span></p>",
            "sensitive": true,
            "attachment": [
                {
                    "type": "Document",
                    "mediaType": "image/webp",
                    "url": "https://media.misskeycdn.com/abc.webp",
                    "name": null,
                    "sensitive": true
                }
            ]
        });

        let obj: ActivityPubObject = serde_json::from_value(json).unwrap();
        let meta = ap_object_to_metadata(&obj, Some("あるるも (@arurumo@misskey.io)")).unwrap();

        assert_eq!(
            meta.title.as_deref(),
            Some("あるるも (@arurumo@misskey.io)")
        );
        // CW is folded into the description.
        assert_eq!(
            meta.description.as_deref(),
            Some("CW: NSFW\nsensitive art post")
        );
        assert_eq!(
            meta.image_url,
            Some(Url::parse("https://media.misskeycdn.com/abc.webp").unwrap())
        );
    }
}
