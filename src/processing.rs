use crate::config::Config;
use crate::media::{generate_blurhash, generate_thumbnail, probe_media};
use crate::metadata::Metadata;
use anyhow::{Result, bail};
use matrix_sdk::attachment::AttachmentConfig;
use matrix_sdk::attachment::{BaseImageInfo, Thumbnail};
use matrix_sdk::ruma::events::room::message::TextMessageEventContent;
use mime_guess::Mime;
use reqwest::Url;
use std::io::Write;
use tracing::{debug, warn};

pub struct MessageParams {
    pub body: String,
    pub html_body: String,
    pub media_url: Option<Url>,
}

pub struct AttachmentData {
    pub filename: String,
    pub mime_type: Mime,
    pub data: Vec<u8>,
    pub attachment_config: AttachmentConfig,
}

pub fn process_metadata(meta: Metadata) -> MessageParams {
    let mut media_url = meta.video_url.or(meta.audio_url).or(meta.image_url);

    if meta.card == Some("summary".to_string()) {
        media_url = None;
    }

    let body = format!(
        "{}{}",
        meta.title
            .clone()
            .map(|s| format!("{}", s))
            .unwrap_or_default(),
        meta.description
            .clone()
            .map(|s| format!(": {}", s))
            .unwrap_or_default()
    );

    let html_title = meta.title.clone().map(|s| {
        let escaped = html_escape::encode_text(&s);
        escaped.replace('\n', "<br/>")
    });

    let html_desc = meta.description.clone().map(|s| {
        let escaped = html_escape::encode_text(&s);
        escaped.replace('\n', "<br/>")
    });

    let html_body = format!(
        "{}<blockquote>{}{}</blockquote>",
        if media_url.is_some() { "<br/>" } else { "" },
        html_title
            .map(|s| format!(
                "<strong>{}{}</strong>",
                s,
                if meta.description.is_some() { ":" } else { "" }
            ))
            .unwrap_or_default(),
        html_desc
            .map(|s| format!("<p>{}</p>", s))
            .unwrap_or_default(),
    );

    MessageParams {
        body,
        html_body,
        media_url,
    }
}

pub async fn process_response(
    mut response: reqwest::Response,
    config: &Config,
    text: Option<TextMessageEventContent>,
) -> Result<AttachmentData> {
    let content_length = response.content_length();
    if let Some(len) = content_length {
        if len > config.max_file_size {
            bail!("File too large based on Content-Length: {}", len);
        }
    }

    let mut mime_type: Mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(mime_guess::from_path(response.url().path()).first_or_octet_stream());

    let content_disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let final_url = response.url().clone();

    let mut tmp_file = tempfile::NamedTempFile::new()?;
    let mut downloaded: u64 = 0;

    while let Some(chunk) = response.chunk().await? {
        downloaded += chunk.len() as u64;
        if downloaded > config.max_file_size {
            bail!("File too large (streamed): {}", downloaded);
        }
        tmp_file.write_all(&chunk)?;
    }

    let path = tmp_file.path();
    let data = tokio::fs::read(path).await?;

    // Sniff MIME type from content
    if let Some(kind) = infer::get(&data) {
        debug!("Sniffed MIME type from content: {}", kind.mime_type());
        if let Ok(sniffed) = kind.mime_type().parse::<Mime>() {
            mime_type = sniffed;
        }
    }

    debug!("Final MIME type: {}", mime_type);

    let mime_extensions = mime_guess::get_mime_extensions(&mime_type);
    let preferred_extension = mime_extensions
        .and_then(|exts| exts.first())
        .map(|s| s.to_string());

    if let Some(ext) = &preferred_extension {
        debug!("Preferred extension: {}", ext);
    } else {
        debug!("No preferred extension found for MIME type: {}", mime_type);
    }

    let mut attachment_config = AttachmentConfig::new();

    match probe_media(&data) {
        Ok(info) => {
            let mut base_info = BaseImageInfo::default();
            base_info.width = Some(info.width.into());
            base_info.height = Some(info.height.into());
            debug!("Dimensions: {}x{}", info.width, info.height);

            let mut thumbnail_data = None;
            let mut blurhash = None;

            if let Ok(thumb) = generate_thumbnail(&data, 600) {
                thumbnail_data = Some(thumb.clone());
                debug!("Thumbnail generated");

                if let Ok(bh) = generate_blurhash(&thumb) {
                    debug!("Blurhash: {}", bh.clone());
                    blurhash = Some(bh);
                }
            }

            base_info.blurhash = blurhash;

            if let Some(thumb) = thumbnail_data {
                let thumb_mime: Mime = "image/jpeg".parse().unwrap();
                let (thumb_width, thumb_height) = if let Ok(info) = probe_media(&thumb) {
                    (Some(info.width.into()), Some(info.height.into()))
                } else {
                    (None, None)
                };

                if let (Some(w), Some(h)) = (thumb_width, thumb_height) {
                    let thumbnail = Thumbnail {
                        data: thumb.clone(),
                        content_type: thumb_mime,
                        width: w,
                        height: h,
                        size: (thumb.len() as u32).into(),
                    };
                    attachment_config = attachment_config.thumbnail(Some(thumbnail));
                    debug!("Thumbnail added");
                }
            }

            // Add the info to the specific config type
            if mime_type.type_() == mime_guess::mime::IMAGE {
                attachment_config = attachment_config
                    .info(matrix_sdk::attachment::AttachmentInfo::Image(base_info));
            } else if mime_type.type_() == mime_guess::mime::VIDEO {
                let mut video_info = matrix_sdk::attachment::BaseVideoInfo::default();
                video_info.width = Some(info.width.into());
                video_info.height = Some(info.height.into());
                video_info.blurhash = base_info.blurhash;

                attachment_config = attachment_config
                    .info(matrix_sdk::attachment::AttachmentInfo::Video(video_info));
            } else if mime_type.type_() == mime_guess::mime::AUDIO {
                let audio_info = matrix_sdk::attachment::BaseAudioInfo::default();
                attachment_config = attachment_config
                    .info(matrix_sdk::attachment::AttachmentInfo::Audio(audio_info));
            }
        }
        Err(e) => {
            warn!("Failed to probe media: {}", e);
        }
    }

    // Determine filename
    let mut filename = if let Some(ext) = &preferred_extension {
        format!("media.{}", ext)
    } else {
        "media".to_string()
    };

    let mut found_name = None;
    if let Some(cd) = content_disposition {
        for part in cd.split(';') {
            let part = part.trim();
            if part.to_lowercase().starts_with("filename=") {
                let mut name = part["filename=".len()..].trim().to_string();
                if name.starts_with('"') && name.ends_with('"') {
                    name = name[1..name.len() - 1].to_string();
                }
                if !name.is_empty() {
                    found_name = Some(name);
                    break;
                }
            }
        }
    }

    if found_name.is_none() {
        if let Some(name) = final_url
            .path_segments()
            .and_then(|segments| segments.last())
        {
            if !name.is_empty() {
                found_name = Some(name.to_string());
            }
        }
    }

    if let Some(mut name) = found_name {
        // Enforce extension if we have a preferred one.
        // This basically ensures that we don't accidentally use .jpg:large or similar,
        // which happens with Twitter, and Element doesn't like it.
        if let Some(ext) = &preferred_extension {
            let path = std::path::Path::new(&name);
            let current_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let is_valid_ext = if let Some(valid_exts) = mime_extensions {
                valid_exts
                    .iter()
                    .any(|&x| x.eq_ignore_ascii_case(current_ext))
            } else {
                false
            };

            if !is_valid_ext {
                name = format!("{}.{}", name, ext);
            }
        }
        filename = name;
        debug!("Discovered filename: {}", filename);
    } else {
        debug!("Using fallback filename: {}", filename);
    }

    if let Some(caption) = text {
        attachment_config = attachment_config.caption(Some(caption));
    }

    Ok(AttachmentData {
        filename,
        mime_type,
        data,
        attachment_config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_process_metadata() {
        let meta = Metadata {
            card: Some("summary_large_image".to_string()),
            title: Some("Test Title".to_string()),
            description: Some("Test Description".to_string()),
            image_url: None,
            video_url: Some(Url::parse("https://example.com/video.mp4").unwrap()),
            audio_url: None,
        };

        let params = process_metadata(meta);

        assert_eq!(params.body, "Test Title: Test Description");
        assert!(params.html_body.contains("<strong>Test Title:</strong>"));
        assert!(params.html_body.contains("<p>Test Description</p>"));
        assert_eq!(
            params.media_url.unwrap().as_str(),
            "https://example.com/video.mp4"
        );
    }

    #[tokio::test]
    async fn test_process_response_video() {
        let mock_server = MockServer::start().await;

        // Read test file
        let file_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/big_buck_bunny.webm");
        let file_content = std::fs::read(file_path).expect("Failed to read test file");

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(file_content))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let uri = mock_server.uri();
        let response = client
            .get(&uri)
            .send()
            .await
            .expect("Failed to send request");

        let config = Config {
            max_file_size: 10 * 1024 * 1024,
            ..Config::default()
        };

        let attachment = process_response(response, &config, None)
            .await
            .expect("Failed to process response");

        assert_eq!(attachment.mime_type.to_string(), "video/webm");
        assert_eq!(attachment.filename, "media.webm");
    }
}
