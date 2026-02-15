use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use url::Url;

use crate::auth::Credentials;
use crate::probe::{Protocol, SpeedProbeResult};

#[derive(Debug, Clone)]
pub struct TransferPlan {
    pub chosen_url: Url,
    pub output_dir: PathBuf,
    pub output_filename: String,
    pub max_connections: u32,
    pub split: u32,
    pub min_split_size: String,
    pub credentials: Option<Credentials>,
}

impl TransferPlan {
    pub fn output_path(&self) -> PathBuf {
        self.output_dir.join(&self.output_filename)
    }

    pub fn aria2_args(&self) -> Vec<String> {
        let mut args = vec![
            self.chosen_url.to_string(),
            "--continue=true".to_string(),
            "--allow-overwrite=false".to_string(),
            "--auto-file-renaming=false".to_string(),
            "--file-allocation=none".to_string(),
            "--summary-interval=1".to_string(),
            "--console-log-level=warn".to_string(),
            "--retry-wait=2".to_string(),
            "--max-tries=0".to_string(),
            "--timeout=30".to_string(),
            "--enable-http-pipelining=true".to_string(),
            format!("--max-connection-per-server={}", self.max_connections),
            format!("--split={}", self.split),
            format!("--min-split-size={}", self.min_split_size),
            format!("--dir={}", self.output_dir.display()),
            format!("--out={}", self.output_filename),
        ];

        if self.chosen_url.scheme() == "ftp" {
            args.push("--ftp-type=binary".to_string());
        }

        if let Some(creds) = &self.credentials {
            match self.protocol() {
                Protocol::Http => {
                    args.push(format!("--http-user={}", creds.username));
                    args.push(format!("--http-passwd={}", creds.password));
                }
                Protocol::Ftp => {
                    args.push(format!("--ftp-user={}", creds.username));
                    args.push(format!("--ftp-passwd={}", creds.password));
                }
                Protocol::Unknown => {}
            }
        }

        args
    }

    pub fn protocol(&self) -> Protocol {
        Protocol::from_scheme(self.chosen_url.scheme())
    }
}

pub fn build_transfer_plan(
    chosen_url: Url,
    out: Option<PathBuf>,
    filename_override: Option<String>,
    max_connections_override: Option<u32>,
    probe: Option<SpeedProbeResult>,
    credentials: Option<Credentials>,
) -> Result<TransferPlan> {
    let (output_dir, output_filename) = resolve_output_target(&chosen_url, out, filename_override)?;

    let baseline_mbps = probe.as_ref().map(|p| p.mbps);
    let adaptive = choose_adaptive_connection_count(baseline_mbps);
    let max_connections = max_connections_override.unwrap_or(adaptive);

    let split = max_connections.clamp(1, 32);

    Ok(TransferPlan {
        chosen_url,
        output_dir,
        output_filename,
        max_connections: max_connections.clamp(1, 32),
        split,
        min_split_size: "8M".to_string(),
        credentials,
    })
}

fn resolve_output_target(
    url: &Url,
    out: Option<PathBuf>,
    filename_override: Option<String>,
) -> Result<(PathBuf, String)> {
    let url_filename = file_name_from_url(url).unwrap_or_else(|| "download.bin".to_string());

    match (out, filename_override) {
        (None, None) => {
            let cwd = std::env::current_dir().context("failed to read current directory")?;
            Ok((cwd, url_filename))
        }
        (None, Some(filename)) => {
            let cwd = std::env::current_dir().context("failed to read current directory")?;
            Ok((cwd, filename))
        }
        (Some(out_path), Some(filename)) => {
            ensure_directory(&out_path)?;
            Ok((out_path, filename))
        }
        (Some(out_path), None) => {
            if out_path.exists() && out_path.is_dir() {
                return Ok((out_path, url_filename));
            }

            if out_path.extension().is_some() {
                let parent = out_path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                ensure_directory(&parent)?;
                let filename = out_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| anyhow!("invalid output filename"))?
                    .to_string();
                return Ok((parent, filename));
            }

            // Path does not exist and has no extension: treat as directory.
            std::fs::create_dir_all(&out_path)
                .with_context(|| format!("failed to create output dir {}", out_path.display()))?;
            Ok((out_path, url_filename))
        }
    }
}

fn ensure_directory(path: &Path) -> Result<()> {
    if path.exists() {
        if !path.is_dir() {
            return Err(anyhow!("{} is not a directory", path.display()));
        }
        return Ok(());
    }

    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create output dir {}", path.display()))?;
    Ok(())
}

fn file_name_from_url(url: &Url) -> Option<String> {
    url.path_segments()
        .and_then(|segments| segments.filter(|s| !s.is_empty()).last())
        .map(str::to_string)
}

pub fn choose_adaptive_connection_count(mbps: Option<f64>) -> u32 {
    match mbps {
        Some(speed) if speed >= 200.0 => 16,
        Some(speed) if speed >= 120.0 => 12,
        Some(speed) if speed >= 40.0 => 8,
        Some(_) => 4,
        None => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_connection_count() {
        assert_eq!(choose_adaptive_connection_count(Some(220.0)), 16);
        assert_eq!(choose_adaptive_connection_count(Some(130.0)), 12);
        assert_eq!(choose_adaptive_connection_count(Some(80.0)), 8);
        assert_eq!(choose_adaptive_connection_count(Some(10.0)), 4);
        assert_eq!(choose_adaptive_connection_count(None), 8);
    }

    #[test]
    fn infers_filename_from_url() {
        let url = Url::parse("https://example.com/path/movie.mkv").expect("valid url");
        assert_eq!(file_name_from_url(&url), Some("movie.mkv".to_string()));
    }
}
