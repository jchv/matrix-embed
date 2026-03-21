use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

const DEFAULT_COMMAND_PREFIX: &str = "!embedbot";
const DEFAULT_HOMESERVER_URL: &str = "https://matrix.org";
const DEFAULT_STATE_STORE_PATH: &str = "state";
const DEFAULT_DATABASE_PATH: &str = "matrix-embed.db";
const DEFAULT_MEDIA_STORE_PATH: &str = "media";
const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100 MB
const DEFAULT_DOWNLOAD_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_MAX_EMBED_DESCRIPTION_CHARS: usize = 640;
const DEFAULT_MAX_EMBED_DESCRIPTION_LINES: usize = 8;

fn default_ignored_title_patterns() -> Vec<Regex> {
    vec![Regex::new(r"^(Image|Video|Audio) File$").unwrap()]
}

fn default_ignored_url_patterns() -> Vec<Regex> {
    vec![Regex::new(r"^https?://(www\.)?matrix\.to/").unwrap()]
}

fn default_url_rewrites() -> Vec<(regex::Regex, String)> {
    vec![
        (
            Regex::new(r"^https?://(www\.)?x(cancel)?\.com/").unwrap(),
            "https://vxtwitter.com/".to_string(),
        ),
        (
            // fixupx/fxembed doesn't seem to work very well. Let's just rewrite it to vxtwitter too.
            Regex::new(r"^https?://(www\.)?fixupx?\.com/").unwrap(),
            "https://vxtwitter.com/".to_string(),
        ),
        (
            Regex::new(r"^https?://(www\.)?pixiv\.net/").unwrap(),
            "https://phixiv.net/".to_string(),
        ),
        (
            Regex::new(r"^https?://(www\.)?instagram\.com/").unwrap(),
            "https://www.kkinstagram.com/".to_string(),
        ),
    ]
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(long, default_value = DEFAULT_HOMESERVER_URL)]
    pub homeserver_url: Url,

    #[arg(long)]
    pub username: Option<String>,

    /// Path to a file containing the password
    #[arg(long)]
    pub password_file: Option<PathBuf>,

    #[arg(long, default_value = DEFAULT_STATE_STORE_PATH)]
    pub state_store_path: PathBuf,

    /// Max file size in bytes
    #[arg(long, default_value_t = DEFAULT_MAX_FILE_SIZE)]
    pub max_file_size: u64,

    /// Download timeout in seconds
    #[arg(long, default_value_t = DEFAULT_DOWNLOAD_TIMEOUT_SECONDS)]
    pub download_timeout_seconds: u64,

    /// Trusted users who can invite the bot (can be specified multiple times)
    #[arg(long)]
    pub trusted_users: Vec<String>,

    /// Path to a JSON file containing URL rewrite rules
    #[arg(long)]
    pub url_rewrites_file: Option<PathBuf>,

    /// Path to the SQLite database for persistent bot state
    #[arg(long, default_value = DEFAULT_DATABASE_PATH)]
    pub database_path: PathBuf,

    /// Path to the content-addressable media store directory
    #[arg(long, default_value = DEFAULT_MEDIA_STORE_PATH)]
    pub media_store_path: PathBuf,

    /// Path to avatar to set, if none is set
    #[arg(long)]
    pub avatar_file: Option<PathBuf>,

    /// Display name to set on the bot's profile
    #[arg(long)]
    pub display_name: Option<String>,

    /// Command prefix the bot responds to (e.g. "!mybot")
    #[arg(long, default_value = DEFAULT_COMMAND_PREFIX)]
    pub command_prefix: String,

    /// Proxy to use when making external requests
    #[arg(long)]
    pub proxy: Option<Url>,

    /// Reset identity
    #[arg(long)]
    pub reset_identity: bool,

    /// Path to a file containing the recovery passphrase
    #[arg(long)]
    pub recovery_passphrase_file: Option<PathBuf>,

    /// Regular expressions for og:title values that should be ignored (can be specified multiple times)
    #[arg(long)]
    pub ignored_title_pattern: Vec<String>,

    /// Regular expressions for URLs that should be skipped entirely (can be specified multiple times)
    #[arg(long)]
    pub ignored_url_pattern: Vec<String>,

    /// Maximum number of characters allowed in an embed description
    #[arg(long, default_value_t = DEFAULT_MAX_EMBED_DESCRIPTION_CHARS)]
    pub max_embed_description_chars: usize,

    /// Maximum number of lines allowed in an embed description
    #[arg(long, default_value_t = DEFAULT_MAX_EMBED_DESCRIPTION_LINES)]
    pub max_embed_description_lines: usize,
}

#[derive(Debug, Deserialize, Default)]
struct RewriteConfig {
    regex: String,
    replacement: String,
}

#[derive(Debug)]
pub struct Config {
    pub homeserver_url: Url,
    pub username: String,
    pub password: Option<String>,
    pub state_store_path: PathBuf,
    pub database_path: PathBuf,
    pub media_store_path: PathBuf,
    pub max_file_size: u64,
    pub download_timeout: Duration,
    pub trusted_users: Vec<String>,
    pub url_rewrites: Vec<(regex::Regex, String)>,
    pub ignored_title_patterns: Vec<Regex>,
    pub ignored_url_patterns: Vec<Regex>,
    pub max_embed_description_chars: usize,
    pub max_embed_description_lines: usize,
    pub avatar_data: Option<Vec<u8>>,
    pub display_name: Option<String>,
    pub command_prefix: String,
    pub proxy: Option<Url>,
    pub reset_identity: bool,
    pub recovery_passphrase: Option<String>,
}

impl Config {
    pub async fn load() -> Result<Self> {
        let args = Args::parse();

        let password = if let Some(path) = args.password_file {
            Some(
                tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to read password file: {:?}", path))?
                    .trim()
                    .to_string(),
            )
        } else {
            None
        };

        let url_rewrites = if let Some(path) = args.url_rewrites_file {
            let content = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to read URL rewrites file: {:?}", path))?;
            let rewrites: Vec<RewriteConfig> = serde_json::from_str(&content)
                .with_context(|| "Failed to parse URL rewrites file")?;

            rewrites
                .into_iter()
                .map(|r| {
                    let re = Regex::new(&r.regex)
                        .with_context(|| format!("Invalid regex: {}", r.regex))?;
                    Ok((re, r.replacement))
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            default_url_rewrites()
        };

        let ignored_title_patterns = if args.ignored_title_pattern.is_empty() {
            default_ignored_title_patterns()
        } else {
            args.ignored_title_pattern
                .iter()
                .map(|p| {
                    Regex::new(p)
                        .with_context(|| format!("Invalid ignored title pattern regex: {}", p))
                })
                .collect::<Result<Vec<_>>>()?
        };

        let ignored_url_patterns = if args.ignored_url_pattern.is_empty() {
            default_ignored_url_patterns()
        } else {
            args.ignored_url_pattern
                .iter()
                .map(|p| {
                    Regex::new(p)
                        .with_context(|| format!("Invalid ignored URL pattern regex: {}", p))
                })
                .collect::<Result<Vec<_>>>()?
        };

        let recovery_passphrase = if let Some(path) = args.recovery_passphrase_file {
            Some(
                tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| {
                        format!("Failed to read recovery passphrase file: {:?}", path)
                    })?
                    .trim()
                    .to_string(),
            )
        } else {
            None
        };

        let avatar_data = if let Some(path) = args.avatar_file {
            Some(
                tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("Failed to read avatar file: {:?}", path))?,
            )
        } else {
            None
        };

        Ok(Self {
            homeserver_url: args.homeserver_url,
            username: args.username.unwrap_or_default(),
            password,
            state_store_path: args.state_store_path,
            database_path: args.database_path,
            media_store_path: args.media_store_path,
            max_file_size: args.max_file_size,
            download_timeout: Duration::from_secs(args.download_timeout_seconds),
            trusted_users: args.trusted_users,
            url_rewrites,
            ignored_title_patterns,
            ignored_url_patterns,
            max_embed_description_chars: args.max_embed_description_chars,
            max_embed_description_lines: args.max_embed_description_lines,
            avatar_data,
            display_name: args.display_name,
            command_prefix: args.command_prefix,
            proxy: args.proxy,
            reset_identity: args.reset_identity,
            recovery_passphrase,
        })
    }

    pub fn is_url_ignored(&self, url: &Url) -> bool {
        let url_str = url.as_str();
        self.ignored_url_patterns
            .iter()
            .any(|re| re.is_match(url_str))
    }

    pub fn rewrite_url(&self, url: &Url) -> Url {
        let url_str = url.as_str();
        for (regex, replacement) in &self.url_rewrites {
            let new_url_str = regex.replace(url_str, replacement.as_str());
            if new_url_str != url_str
                && let Ok(new_url) = Url::parse(&new_url_str)
            {
                return new_url;
            }
        }
        url.clone()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            homeserver_url: Url::parse(DEFAULT_HOMESERVER_URL).unwrap(),
            username: "".to_string(),
            password: None,
            state_store_path: PathBuf::from(DEFAULT_STATE_STORE_PATH),
            database_path: PathBuf::from(DEFAULT_DATABASE_PATH),
            media_store_path: PathBuf::from(DEFAULT_MEDIA_STORE_PATH),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            download_timeout: Duration::from_secs(DEFAULT_DOWNLOAD_TIMEOUT_SECONDS),
            trusted_users: vec![],
            url_rewrites: default_url_rewrites(),
            ignored_title_patterns: default_ignored_title_patterns(),
            ignored_url_patterns: default_ignored_url_patterns(),
            max_embed_description_chars: DEFAULT_MAX_EMBED_DESCRIPTION_CHARS,
            max_embed_description_lines: DEFAULT_MAX_EMBED_DESCRIPTION_LINES,
            avatar_data: None,
            display_name: None,
            command_prefix: DEFAULT_COMMAND_PREFIX.to_string(),
            proxy: None,
            reset_identity: false,
            recovery_passphrase: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_url() {
        let config = Config::default();

        let url = Url::parse("https://x.com/what/ever").unwrap();
        let new_url = config.rewrite_url(&url);
        assert_eq!(new_url.as_str(), "https://vxtwitter.com/what/ever");

        let url = Url::parse("https://www.x.com/what/ever").unwrap();
        let new_url = config.rewrite_url(&url);
        assert_eq!(new_url.as_str(), "https://vxtwitter.com/what/ever");

        let url = Url::parse("https://google.com").unwrap();
        let new_url = config.rewrite_url(&url);
        assert_eq!(new_url.as_str(), "https://google.com/");
    }

    #[test]
    fn test_is_url_ignored() {
        let config = Config::default();

        assert!(
            config.is_url_ignored(&Url::parse("https://matrix.to/#/@user:example.com").unwrap())
        );
        assert!(
            config
                .is_url_ignored(&Url::parse("https://www.matrix.to/#/!room:example.com").unwrap())
        );
        assert!(!config.is_url_ignored(&Url::parse("https://example.com/page").unwrap()));
        assert!(!config.is_url_ignored(&Url::parse("https://notmatrix.to/something").unwrap()));
    }
}
