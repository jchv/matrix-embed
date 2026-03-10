use crate::config::Config;
use matrix_sdk::ruma::events::room::message::TextMessageEventContent;
use scraper::{Html, Selector};
use std::{collections::HashSet, sync::LazyLock};
use tracing::debug;
use url::Url;

/// Used to find links inside of the quoted <mx-reply>. Look at the comment for
/// [extract_quoted_urls] for more information.
static QUOTED_LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("mx-reply a[href]").unwrap());

/// Used to find the <mx-reply> element itself so we can scan its text nodes
/// for plain-text URLs (i.e. URLs not wrapped in an <a> tag).
static QUOTED_REPLY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("mx-reply").unwrap());

/// Extract URLs from the quoted message of a formatted body.
///
/// Some Matrix clients embed a quoted message in the <mx-reply> in the body of
/// a message. We actually parse the unformatted raw `body` when looking for
/// URLs, but for clients that embed the quoted message in the body, the
/// unformatted body doesn't distinguish between text from the quoted message
/// and text from the actual message. The formatted body does, but it erases an
/// important bit of syntax that we want to keep: the angular brackets around
/// URLs. We ignore URLs with the angular brackets, similar to Discord previews.
///
/// In order to work around this problem, when the formatted body contains URLs
/// inside of an mx-reply, we collect those into a hash set and when we come
/// across them in the unformatted body, we ignore them. This isn't perfect, but
/// it should offer a pretty good workaround for the problem.
///
/// So far this seems to only impact Fluffychat, but there might be others.
fn extract_quoted_urls(formatted_body: &str) -> HashSet<Url> {
    let doc = Html::parse_fragment(formatted_body);

    // Collect URLs from <a href> tags within mx-reply.
    let mut urls: HashSet<Url> = doc
        .select(&QUOTED_LINKS)
        .filter_map(|el| el.value().attr("href"))
        .filter_map(|href| Url::parse(href).ok())
        .collect();

    // Collect verbatim URLs as well.
    for reply_el in doc.select(&QUOTED_REPLY) {
        for text in reply_el.text() {
            for word in text.split_whitespace() {
                if (word.starts_with("http://") || word.starts_with("https://"))
                    && let Ok(url) = Url::parse(word)
                {
                    urls.insert(url);
                }
            }
        }
    }

    urls
}

/// Extract a suitable URL to embed from the message. For now, this only ever
/// extracts a single message.
pub fn extract_url(text: &TextMessageEventContent, config: &Config) -> Option<Url> {
    // Collect URLs from the formatted body's <mx-reply> so we can ignore
    // links that belong to the quoted message.
    let reply_urls = text
        .formatted
        .as_ref()
        .map(|f| extract_quoted_urls(&f.body))
        .unwrap_or_default();

    for word in text.body.split_whitespace() {
        if (word.starts_with("http://") || word.starts_with("https://"))
            && let Ok(url) = Url::parse(word)
        {
            if reply_urls.contains(&url) {
                debug!("Skipping URL found in reply: {}", url);
                continue;
            }

            if config.is_url_ignored(&url) {
                debug!("Ignoring URL (matched ignored pattern): {}", url);
                continue;
            }

            // Apply URL rewrites. Return first URL for now?
            return Some(config.rewrite_url(&url));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_quoted_urls_empty_string() {
        let urls = extract_quoted_urls("");
        assert!(urls.is_empty());
    }

    #[test]
    fn test_extract_quoted_urls_no_mx_reply() {
        let html = r#"Hello <a href="https://example.com">link</a>"#;
        let urls = extract_quoted_urls(html);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_extract_quoted_urls_single_url() {
        let html = r#"<mx-reply><blockquote><a href="https://matrix.to/#/@user:example.com">@user</a><br>Check out <a href="https://example.com/page">https://example.com/page</a></blockquote></mx-reply>My reply message"#;
        let urls = extract_quoted_urls(html);
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&Url::parse("https://matrix.to/#/@user:example.com").unwrap()));
        assert!(urls.contains(&Url::parse("https://example.com/page").unwrap()));
    }

    #[test]
    fn test_extract_quoted_urls_ignores_links_outside_mx_reply() {
        let html = r#"<mx-reply><blockquote><a href="https://quoted.example.com">link</a></blockquote></mx-reply>See <a href="https://reply.example.com">this</a>"#;
        let urls = extract_quoted_urls(html);
        assert_eq!(urls.len(), 1);
        assert!(urls.contains(&Url::parse("https://quoted.example.com").unwrap()));
        assert!(!urls.contains(&Url::parse("https://reply.example.com").unwrap()));
    }

    #[test]
    fn test_extract_quoted_urls_html_entities_decoded() {
        let html = r#"<mx-reply><blockquote><a href="https://example.com/search?a=1&amp;b=2">link</a></blockquote></mx-reply>"#;
        let urls = extract_quoted_urls(html);
        assert_eq!(urls.len(), 1);
        // It's important to make sure that the result we get has the HTML entities decoded.
        // This happens by virtue of parsing the HTML, so we don't actually need to do anything special to get this behavior.
        // However, the implementation may change for one reason or another, so let's make sure we test it.
        assert!(urls.contains(&Url::parse("https://example.com/search?a=1&b=2").unwrap()));
    }

    #[test]
    fn test_extract_quoted_urls_invalid_href_skipped() {
        // We don't want to crash just because a URL is invalid; let's just make sure we skip over them.
        let html = r#"<mx-reply><blockquote><a href="not a url">bad</a> and <a href="https://good.example.com">good</a></blockquote></mx-reply>"#;
        let urls = extract_quoted_urls(html);
        assert_eq!(urls.len(), 1);
        assert!(urls.contains(&Url::parse("https://good.example.com").unwrap()));
    }

    #[test]
    fn test_extract_quoted_urls_no_links_in_mx_reply() {
        let html = r#"<mx-reply><blockquote>Just plain text</blockquote></mx-reply>"#;
        let urls = extract_quoted_urls(html);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_extract_quoted_urls_plain_text_url_no_anchor() {
        let html = r#"<mx-reply><blockquote><a href="https://matrix.to/#/!room/$event">In reply to</a> <a href="https://matrix.to/#/@user:matrix.org">@user:matrix.org</a><br>https:&#47;&#47;x.com&#47;user&#47;status&#47;1234567890123456789</blockquote></mx-reply>Reply"#;
        let urls = extract_quoted_urls(html);
        assert!(
            urls.contains(&Url::parse("https://x.com/user/status/1234567890123456789").unwrap())
        );
    }

    #[test]
    fn test_extract_url_ignore_quoted_plain_text_url() {
        assert_eq!(
            extract_url(
                &TextMessageEventContent::html(
                    "> <@user:matrix.org> https://x.com/user/status/1234567890123456789\n\nReply",
                    r#"<mx-reply><blockquote><a href="https://matrix.to/#/!room/$event">In reply to</a> <a href="https://matrix.to/#/@user:matrix.org">@user:matrix.org</a><br>https:&#47;&#47;x.com&#47;user&#47;status&#47;1234567890123456789</blockquote></mx-reply>Reply"#
                ),
                &Default::default(),
            ),
            None
        );
    }

    #[test]
    fn test_extract_url_empty_string() {
        assert_eq!(
            extract_url(&TextMessageEventContent::plain(""), &Default::default()),
            None
        );
    }

    #[test]
    fn test_extract_url_basic_url() {
        assert_eq!(
            extract_url(
                &TextMessageEventContent::plain("https://example.com"),
                &Default::default(),
            ),
            Some(Url::parse("https://example.com").unwrap())
        );
    }

    #[test]
    fn test_extract_url_ignore_bracketed() {
        assert_eq!(
            extract_url(
                &TextMessageEventContent::plain(
                    "<https://ignored.example.com> https://accepted.example.com"
                ),
                &Default::default(),
            ),
            Some(Url::parse("https://accepted.example.com").unwrap())
        );
    }

    #[test]
    fn test_extract_url_ignore_quoted() {
        assert_eq!(
            extract_url(
                &TextMessageEventContent::html(
                    "> https://quoted.example.com\n\nSee https://reply.example.com",
                    r#"<mx-reply><blockquote><a href="https://quoted.example.com">https://quoted.example.com</a></blockquote></mx-reply>See <a href="https://reply.example.com">https://reply.example.com</a>"#
                ),
                &Default::default(),
            ),
            Some(Url::parse("https://reply.example.com").unwrap())
        );
    }
}
