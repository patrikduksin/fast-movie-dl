use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use url::Url;

use crate::cli::ProtocolArg;
use crate::errors::AppError;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Protocol {
    Http,
    Ftp,
    Unknown,
}

impl Protocol {
    pub fn from_scheme(scheme: &str) -> Self {
        match scheme {
            "http" | "https" => Self::Http,
            "ftp" => Self::Ftp,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UrlCandidate {
    pub url: Url,
    pub protocol: Protocol,
}

#[derive(Debug, Clone)]
pub struct SpeedProbeResult {
    pub protocol: Protocol,
    pub mbps: f64,
    pub sample_bytes: u64,
    pub sample_seconds: f64,
}

#[derive(Debug, Clone)]
pub struct SpeedProbeAttempt {
    pub protocol: Protocol,
    pub result: Option<SpeedProbeResult>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct ProbeSelection {
    pub chosen: UrlCandidate,
    pub best_probe: Option<SpeedProbeResult>,
    pub all_probes: Vec<SpeedProbeResult>,
    pub reason: String,
}

pub fn resolve_candidates(
    primary_url: &str,
    mode: ProtocolArg,
    explicit_http_url: Option<&str>,
    explicit_ftp_url: Option<&str>,
) -> Result<Vec<UrlCandidate>> {
    let primary = Url::parse(primary_url).with_context(|| format!("invalid URL: {primary_url}"))?;
    let primary_protocol = Protocol::from_scheme(primary.scheme());
    if primary_protocol == Protocol::Unknown {
        return Err(AppError::UnsupportedProtocol(primary.scheme().to_string()).into());
    }

    let mut dedup: HashMap<Protocol, UrlCandidate> = HashMap::new();

    if primary_protocol != Protocol::Unknown {
        dedup.insert(
            primary_protocol,
            UrlCandidate {
                url: primary,
                protocol: primary_protocol,
            },
        );
    }

    if let Some(raw) = explicit_http_url {
        let parsed = Url::parse(raw).with_context(|| format!("invalid --http-url: {raw}"))?;
        if Protocol::from_scheme(parsed.scheme()) != Protocol::Http {
            return Err(anyhow!("--http-url must use http or https"));
        }
        dedup.insert(
            Protocol::Http,
            UrlCandidate {
                url: parsed,
                protocol: Protocol::Http,
            },
        );
    }

    if let Some(raw) = explicit_ftp_url {
        let parsed = Url::parse(raw).with_context(|| format!("invalid --ftp-url: {raw}"))?;
        if Protocol::from_scheme(parsed.scheme()) != Protocol::Ftp {
            return Err(anyhow!("--ftp-url must use ftp"));
        }
        dedup.insert(
            Protocol::Ftp,
            UrlCandidate {
                url: parsed,
                protocol: Protocol::Ftp,
            },
        );
    }

    let candidates: Vec<UrlCandidate> = match mode {
        ProtocolArg::Auto => dedup.into_values().collect(),
        ProtocolArg::Http => dedup
            .into_values()
            .filter(|c| c.protocol == Protocol::Http)
            .collect(),
        ProtocolArg::Ftp => dedup
            .into_values()
            .filter(|c| c.protocol == Protocol::Ftp)
            .collect(),
    };

    if candidates.is_empty() {
        return Err(AppError::NoCandidates.into());
    }

    Ok(candidates)
}

pub fn select_candidate_with_probe(
    aria2_path: &str,
    candidates: &[UrlCandidate],
    mode: ProtocolArg,
) -> Result<ProbeSelection> {
    if candidates.is_empty() {
        return Err(AppError::NoCandidates.into());
    }

    if mode != ProtocolArg::Auto || candidates.len() == 1 {
        return Ok(ProbeSelection {
            chosen: candidates[0].clone(),
            best_probe: None,
            all_probes: Vec::new(),
            reason: "single protocol candidate or protocol forced".to_string(),
        });
    }

    let mut probes = Vec::new();

    for candidate in candidates {
        if let Ok(probe) = probe_candidate(aria2_path, candidate, 10) {
            if probe.sample_bytes > 0 {
                probes.push(probe);
            }
        }
    }

    if probes.is_empty() {
        // Could not obtain sample data. Prefer HTTP when available.
        let chosen = candidates
            .iter()
            .find(|c| c.protocol == Protocol::Http)
            .unwrap_or(&candidates[0])
            .clone();
        return Ok(ProbeSelection {
            chosen,
            best_probe: None,
            all_probes: probes,
            reason: "speed probe unavailable; defaulting to HTTP when possible".to_string(),
        });
    }

    let mut sorted = probes.clone();
    sorted.sort_by(|a, b| {
        b.mbps
            .partial_cmp(&a.mbps)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let winner = &sorted[0];
    let runner_up = sorted.get(1);

    let chosen_protocol = if let Some(second) = runner_up {
        // Require a clear lead to switch away from HTTP if close.
        let lead_ratio = if second.mbps > 0.0 {
            (winner.mbps - second.mbps) / second.mbps
        } else {
            1.0
        };

        if lead_ratio < 0.10 {
            Protocol::Http
        } else {
            winner.protocol
        }
    } else {
        winner.protocol
    };

    let chosen = candidates
        .iter()
        .find(|c| c.protocol == chosen_protocol)
        .unwrap_or(&candidates[0])
        .clone();

    let best_probe = probes
        .iter()
        .filter(|p| p.protocol == chosen.protocol)
        .max_by(|a, b| {
            a.mbps
                .partial_cmp(&b.mbps)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();

    Ok(ProbeSelection {
        chosen,
        best_probe,
        all_probes: probes,
        reason: "selected by short aria2 throughput probe".to_string(),
    })
}

pub fn probe_candidate_for_speed_test(
    aria2_path: &str,
    candidate: &UrlCandidate,
    sample_seconds: u64,
) -> Result<SpeedProbeResult> {
    probe_candidate(aria2_path, candidate, sample_seconds)
}

fn probe_candidate(
    aria2_path: &str,
    candidate: &UrlCandidate,
    sample_seconds: u64,
) -> Result<SpeedProbeResult> {
    let temp_root = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis();
    let dir = temp_root.join(format!(
        "fast-movie-dl-probe-{}-{}",
        std::process::id(),
        stamp
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed creating probe directory {}", dir.display()))?;

    let out_name = "probe.bin";
    let out_path = dir.join(out_name);

    let mut command = Command::new(aria2_path);
    command
        .arg(candidate.url.as_str())
        .arg("--continue=true")
        .arg("--allow-overwrite=true")
        .arg("--auto-file-renaming=false")
        .arg("--file-allocation=none")
        .arg("--summary-interval=0")
        .arg("--console-log-level=error")
        .arg("--max-tries=1")
        .arg("--timeout=20")
        .arg("--retry-wait=1")
        .arg("--max-connection-per-server=4")
        .arg("--split=4")
        .arg("--min-split-size=1M")
        .arg(format!("--dir={}", dir.display()))
        .arg(format!("--out={out_name}"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = command.spawn().context("failed to start aria2 probe")?;

    let started = Instant::now();
    thread::sleep(Duration::from_secs(sample_seconds));

    let mut killed_for_timeout = false;
    let status = match child.try_wait()? {
        Some(status) => status,
        None => {
            killed_for_timeout = true;
            let _ = child.kill();
            child
                .wait()
                .context("failed waiting for aria2 probe process")?
        }
    };

    let stderr_text = read_stderr(&mut child);

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let sample_bytes = size_or_zero(&out_path);

    let _ = std::fs::remove_dir_all(&dir);

    if !killed_for_timeout && !status.success() {
        let exit = status.code();
        let exit_text = match exit {
            Some(code) => {
                if let Some(description) = aria2_exit_description(code) {
                    format!("{code}: {description}")
                } else {
                    code.to_string()
                }
            }
            None => "terminated by signal".to_string(),
        };
        let details = summarize_probe_error(&stderr_text);
        return Err(anyhow!(
            "aria2 probe failed (exit code {exit_text}): {details}"
        ));
    }

    if sample_bytes == 0 {
        let details = summarize_probe_error(&stderr_text);
        return Err(anyhow!("no sample bytes downloaded: {details}"));
    }

    Ok(SpeedProbeResult {
        protocol: candidate.protocol,
        mbps: (sample_bytes as f64 * 8.0) / (elapsed * 1_000_000.0),
        sample_bytes,
        sample_seconds: elapsed,
    })
}

fn size_or_zero(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn read_stderr(child: &mut std::process::Child) -> String {
    let mut stderr_text = String::new();

    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut stderr_text);
    }

    stderr_text
}

fn summarize_probe_error(stderr_text: &str) -> String {
    let summary = stderr_text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no additional details from aria2");

    summary.chars().take(220).collect()
}

fn aria2_exit_description(code: i32) -> Option<&'static str> {
    match code {
        3 => Some("resource not found"),
        21 => Some("FTP command failed"),
        24 => Some("HTTP authorization failed"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_auto_candidates_with_dedup() {
        let candidates = resolve_candidates(
            "https://example.com/movie.mkv",
            ProtocolArg::Auto,
            Some("http://example.com/movie.mkv"),
            Some("ftp://example.com/movie.mkv"),
        )
        .expect("expected candidates");

        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.protocol == Protocol::Http));
        assert!(candidates.iter().any(|c| c.protocol == Protocol::Ftp));
    }

    #[test]
    fn filters_forced_protocol() {
        let candidates = resolve_candidates(
            "https://example.com/movie.mkv",
            ProtocolArg::Ftp,
            None,
            Some("ftp://example.com/movie.mkv"),
        )
        .expect("expected candidates");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].protocol, Protocol::Ftp);
    }
}
