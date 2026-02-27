use anyhow::Result;
use scraper::{Html, Selector};
use std::sync::LazyLock;
use url::Url;

// Match both property="og:..." and name="og:..." since some stuff uses name even though it is non-standard.
static OPENGRAPH_SELECTOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"meta[property^="og:"], meta[name^="og:"]"#).unwrap());

// Misskey apparently uses property for twitter meta tags, even though that's only used by opengraph.
static TWITTER_SELECTOR: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(r#"meta[property^="twitter:"], meta[name^="twitter:"]"#).unwrap()
});

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct Metadata {
    pub card: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image_url: Option<Url>,
    pub video_url: Option<Url>,
    pub audio_url: Option<Url>,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        self == &Metadata::default()
    }

    pub async fn fetch_from_url(client: &reqwest::Client, url: &Url) -> Result<Metadata> {
        Ok(Self::parse_from_html(
            &client
                .get(url.clone())
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?,
        ))
    }

    pub fn parse_from_html(html_content: &str) -> Metadata {
        let document = Html::parse_document(html_content);
        let mut metadata = Metadata::default();
        Self::parse_og_meta(&document, &mut metadata);
        Self::parse_twitter_meta(&document, &mut metadata);
        metadata
    }

    fn parse_og_meta(document: &Html, metadata: &mut Metadata) {
        for element in document.select(&OPENGRAPH_SELECTOR) {
            let prop = element
                .value()
                .attr("property")
                .or(element.value().attr("name"));
            let content = element.value().attr("content");

            if let (Some(prop), Some(content)) = (prop, content) {
                match prop {
                    "og:title" => metadata.title = Some(content.to_string()),
                    "og:description" => metadata.description = Some(content.to_string()),
                    "og:image" => {
                        if let Ok(u) = Url::parse(content) {
                            metadata.image_url = Some(u);
                        }
                    }
                    "og:video" => {
                        if let Ok(u) = Url::parse(content) {
                            metadata.video_url = Some(u);
                        }
                    }
                    "og:audio" => {
                        if let Ok(u) = Url::parse(content) {
                            metadata.audio_url = Some(u);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn parse_twitter_meta(document: &Html, metadata: &mut Metadata) {
        for element in document.select(&TWITTER_SELECTOR) {
            if let (Some(name), Some(content)) = (
                element
                    .value()
                    .attr("name")
                    .or(element.value().attr("property")),
                element.value().attr("content"),
            ) {
                match name {
                    "twitter:card" => metadata.card = Some(content.to_string()),
                    "twitter:title" => {
                        if metadata.title.is_none() {
                            metadata.title = Some(content.to_string())
                        }
                    }
                    "twitter:description" => {
                        if metadata.description.is_none() {
                            metadata.description = Some(content.to_string())
                        }
                    }
                    "twitter:image" => {
                        if metadata.image_url.is_none()
                            && let Ok(u) = Url::parse(content)
                        {
                            metadata.image_url = Some(u);
                        }
                    }
                    "twitter:creator" => {
                        if metadata.title.is_none() {
                            let creator = content.to_string();
                            if let Some(creator) = creator.strip_prefix("@") {
                                metadata.title = Some(creator.to_string());
                            } else {
                                metadata.title = Some(creator);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn test_parse_metadata_with_difficult_og_tags() {
        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("tests/data/tweet.html");

        let html_content = fs::read_to_string(d).expect("Failed to read test/data/tweet.html");
        let metadata = Metadata::parse_from_html(&html_content);

        assert_eq!(metadata.title, Some("ebifurako (@_ebi_furako)".to_string()));
        assert_eq!(
            metadata.description,
            Some("前に描いたお気に入りアニメーション".to_string())
        );
        assert_eq!(
            metadata.image_url,
            Some(Url::parse("https://pbs.twimg.com/ext_tw_video_thumb/2021579491018170368/pu/img/iuleedOC8SZIFlOx.jpg").unwrap())
        );
    }
}
