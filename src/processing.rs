use crate::config::Config;
use crate::media::{generate_blurhash, generate_thumbnail, probe_media, remux_to_mp4};
use crate::metadata::Metadata;
use anyhow::{Result, bail};
use matrix_sdk::attachment::{AttachmentConfig, BaseAudioInfo, BaseVideoInfo};
use matrix_sdk::attachment::{BaseImageInfo, Thumbnail};
use matrix_sdk::ruma::events::room::message::TextMessageEventContent;
use mime_guess::Mime;
use reqwest::Url;
use std::io::Write;
use tracing::{debug, info, warn};

#[derive(Debug)]
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

pub fn process_metadata(meta: Metadata, config: &Config) -> MessageParams {
    let media_url = match meta.card.as_deref() {
        Some("summary") => None,
        Some("tweet") => None,
        _ => meta.video_url.or(meta.audio_url).or(meta.image_url),
    };

    // Filter out titles matching any ignored pattern
    let title = meta.title.filter(|t| {
        !config
            .ignored_title_patterns
            .iter()
            .any(|re| re.is_match(t))
    });
    let description = meta.description;
    let has_title = title.is_some();
    let has_desc = description.is_some();

    let body = match (&title, &description) {
        (Some(t), Some(d)) => format!("{}: {}", t, d),
        (Some(t), None) => t.clone(),
        (None, Some(d)) => d.clone(),
        (None, None) => String::new(),
    };

    let html_body = if has_title || has_desc {
        let html_title = title.map(|s| {
            let escaped = html_escape::encode_text(&s);
            escaped.replace('\n', "<br/>")
        });

        let html_desc = description.map(|s| {
            let escaped = html_escape::encode_text(&s);
            escaped.replace('\n', "<br/>")
        });

        format!(
            "{}<blockquote>{}{}</blockquote>",
            if media_url.is_some() { "<br/>" } else { "" },
            html_title
                .map(|s| format!("<strong>{}{}</strong>", s, if has_desc { ":" } else { "" }))
                .unwrap_or_default(),
            html_desc
                .map(|s| format!("<p>{}</p>", s))
                .unwrap_or_default(),
        )
    } else {
        String::new()
    };

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
    if let Some(len) = content_length
        && len > config.max_file_size
    {
        bail!("File too large based on Content-Length: {}", len);
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
    let mut data = tokio::fs::read(path).await?;

    // Sniff MIME type from content
    if let Some(kind) = infer::get(&data) {
        debug!("Sniffed MIME type from content: {}", kind.mime_type());
        if let Ok(sniffed) = kind.mime_type().parse::<Mime>() {
            mime_type = sniffed;
        }
    }

    debug!("Final MIME type: {}", mime_type);

    // Remux Matroska video to MP4 for better client compatibility
    if mime_type == "video/x-matroska" {
        match remux_to_mp4(&data).await {
            Ok(mp4_data) => {
                info!("Successfully remuxed MKV to MP4");
                data = mp4_data;
                mime_type = "video/mp4".parse().unwrap();
            }
            Err(e) => {
                warn!("Failed to remux MKV to MP4, using original: {:?}", e);
            }
        }
    }

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

    match probe_media(&data).await {
        Ok(info) => {
            debug!("Dimensions: {}x{}", info.width, info.height);

            let mut thumbnail_data = None;
            let mut blurhash = None;

            if let Ok(thumb) = generate_thumbnail(&data, 600).await {
                debug!("Thumbnail generated");

                if let Ok(bh) = generate_blurhash(&thumb) {
                    debug!("Blurhash: {}", bh.clone());
                    blurhash = Some(bh);
                }

                thumbnail_data = Some(thumb);
            }

            if let Some(thumb) = thumbnail_data {
                let thumb_mime: Mime = "image/jpeg".parse().unwrap();
                let (thumb_width, thumb_height) = if let Ok(info) = probe_media(&thumb).await {
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
                attachment_config = attachment_config.info(
                    matrix_sdk::attachment::AttachmentInfo::Image(BaseImageInfo {
                        width: Some(info.width.into()),
                        height: Some(info.height.into()),
                        blurhash,
                        ..Default::default()
                    }),
                );
            } else if mime_type.type_() == mime_guess::mime::VIDEO {
                attachment_config = attachment_config.info(
                    matrix_sdk::attachment::AttachmentInfo::Video(BaseVideoInfo {
                        width: Some(info.width.into()),
                        height: Some(info.height.into()),
                        blurhash,
                        ..Default::default()
                    }),
                );
            } else if mime_type.type_() == mime_guess::mime::AUDIO {
                attachment_config = attachment_config.info(
                    matrix_sdk::attachment::AttachmentInfo::Audio(BaseAudioInfo {
                        ..Default::default()
                    }),
                );
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

    if found_name.is_none()
        && let Some(name) = final_url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
        && !name.is_empty()
    {
        found_name = Some(name.to_string());
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

        let params = process_metadata(meta, &Config::default());

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
