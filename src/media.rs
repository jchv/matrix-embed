use anyhow::{Context, Result, bail};
use image::GenericImageView;
use std::io::Write;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

const FFPROBE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const FFPROBE_READ_TIMEOUT: Duration = Duration::from_secs(10);

const FFMPEG_THUMBNAIL_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const FFMPEG_THUMBNAIL_READ_TIMEOUT: Duration = Duration::from_secs(10);

const FFMPEG_REMUX_TIMEOUT: Duration = Duration::from_secs(20);
const FFMPEG_REENCODE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
}

/// Probes media dimensions using ffprobe via stdin/stdout.
/// Runs: ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0 -
pub async fn probe_media(data: &[u8]) -> Result<MediaInfo> {
    let mut child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=s=x:p=0",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn ffprobe")?;

    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = timeout(FFPROBE_WRITE_TIMEOUT, stdin.write_all(data)).await?
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(e).context("Failed to write to ffprobe stdin");
    }

    let output = timeout(FFPROBE_READ_TIMEOUT, child.wait_with_output())
        .await?
        .context("Failed to wait on ffprobe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    if trimmed.is_empty() {
        bail!("ffprobe returned empty output");
    }

    let parts: Vec<&str> = trimmed.split('x').collect();
    if parts.len() != 2 {
        bail!("Unexpected ffprobe output format: {}", trimmed);
    }

    let width = parts[0].parse().context("Failed to parse width")?;
    let height = parts[1].parse().context("Failed to parse height")?;

    Ok(MediaInfo { width, height })
}

/// Generates a thumbnail using ffmpeg via stdin/stdout.
/// Runs: ffmpeg -i - -ss 00:00:00 -vframes 1 -vf scale={target_width}:-1 -f image2 -c:v mjpeg -
pub async fn generate_thumbnail(data: &[u8], target_width: u32) -> Result<Vec<u8>> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "-",
            "-ss",
            "0",
            "-vframes",
            "1",
            "-vf",
            &format!("scale={}:-1", target_width),
            "-f",
            "image2",
            "-c:v",
            "mjpeg",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn ffmpeg")?;

    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = timeout(FFMPEG_THUMBNAIL_WRITE_TIMEOUT, stdin.write_all(data)).await?
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(e).context("Failed to write to ffmpeg stdin");
    }

    let output = timeout(FFMPEG_THUMBNAIL_READ_TIMEOUT, child.wait_with_output())
        .await?
        .context("Failed to wait on ffmpeg")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg failed: {}", stderr);
    }

    Ok(output.stdout)
}

/// Remuxes a Matroska video to MP4 format using ffmpeg.
///
/// First attempts a fast stream-copy remux (`-c copy`). If that fails (e.g.
/// codecs incompatible with the MP4 container), falls back to reencoding with
/// libx264/aac. Uses temporary files so ffmpeg can seek freely (needed for the
/// MP4 moov atom and `-movflags +faststart`).
pub async fn remux_to_mp4(data: &[u8]) -> Result<Vec<u8>> {
    let mut input_file =
        tempfile::NamedTempFile::new().context("Failed to create temp input file for remux")?;
    input_file
        .write_all(data)
        .context("Failed to write input data to temp file for remux")?;
    input_file
        .flush()
        .context("Failed to flush temp input file for remux")?;
    let input_path = input_file.path().to_path_buf();

    let output_file =
        tempfile::NamedTempFile::new().context("Failed to create temp output file for remux")?;
    let output_path = output_file.path().to_path_buf();

    // Attempt 1: fast remux with stream copy (no reencoding)
    let input_str = input_path.to_str().context("Non-UTF8 temp input path")?;
    let output_str = output_path.to_str().context("Non-UTF8 temp output path")?;

    info!("Attempting MKV -> MP4 remux (stream copy)");
    let remux_result = timeout(
        FFMPEG_REMUX_TIMEOUT,
        Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                input_str,
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "-y",
                output_str,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .context("Remux timed out")?
    .context("Failed to run ffmpeg for remux")?;

    if remux_result.status.success() {
        let mp4_data = tokio::fs::read(&output_path)
            .await
            .context("Failed to read remuxed MP4 output")?;
        info!(
            "MKV -> MP4 remux (stream copy) succeeded ({} bytes -> {} bytes)",
            data.len(),
            mp4_data.len()
        );
        return Ok(mp4_data);
    }

    let stderr = String::from_utf8_lossy(&remux_result.stderr);
    warn!(
        "Stream-copy remux failed ({}), falling back to reencode",
        stderr.trim()
    );

    // Attempt 2: reencode with libx264 + aac
    info!("Attempting MKV -> MP4 reencode (libx264/aac)");
    let reencode_result = timeout(
        FFMPEG_REENCODE_TIMEOUT,
        Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                input_str,
                "-c:v",
                "libx264",
                "-preset",
                "fast",
                "-crf",
                "23",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
                "-y",
                output_str,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .context("Reencode timed out")?
    .context("Failed to run ffmpeg for reencode")?;

    if !reencode_result.status.success() {
        let stderr = String::from_utf8_lossy(&reencode_result.stderr);
        bail!("ffmpeg reencode failed: {}", stderr.trim());
    }

    let mp4_data = tokio::fs::read(&output_path)
        .await
        .context("Failed to read reencoded MP4 output")?;
    info!(
        "MKV -> MP4 reencode succeeded ({} bytes -> {} bytes)",
        data.len(),
        mp4_data.len()
    );
    Ok(mp4_data)
}

pub fn generate_blurhash(image_data: &[u8]) -> Result<String> {
    let img = image::load_from_memory(image_data).context("Failed to load image for blurhash")?;
    let (width, height) = img.dimensions();

    blurhash::encode(4, 3, width, height, &img.to_rgba8()).context("Failed to generate blurhash")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn get_test_file_path(filename: &str) -> PathBuf {
        let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        d.push("tests/data");
        d.push(filename);
        d
    }

    #[tokio::test]
    async fn test_probe_media() {
        let path = get_test_file_path("big_buck_bunny.webm");
        let data = fs::read(&path).expect("Failed to read test file");

        let info = probe_media(&data).await.expect("Failed to probe media");
        assert_eq!(info.width, 1280);
        assert_eq!(info.height, 720);
    }

    #[tokio::test]
    async fn test_generate_thumbnail() {
        let path = get_test_file_path("big_buck_bunny.webm");
        let data = fs::read(&path).expect("Failed to read test file");

        let thumb_data = generate_thumbnail(&data, 320)
            .await
            .expect("Failed to generate thumbnail");
        assert!(!thumb_data.is_empty());

        // Verify thumbnail is a valid image and has correct width
        let img = image::load_from_memory(&thumb_data).expect("Failed to load thumbnail as image");
        assert_eq!(img.width(), 320);
    }

    #[tokio::test]
    async fn test_generate_blurhash() {
        // First generate a thumbnail to use for blurhash
        let path = get_test_file_path("big_buck_bunny.webm");
        let data = fs::read(&path).expect("Failed to read test file");
        let thumb_data = generate_thumbnail(&data, 320)
            .await
            .expect("Failed to generate thumbnail");

        let hash = generate_blurhash(&thumb_data).expect("Failed to generate blurhash");
        assert!(!hash.is_empty());
    }
}
