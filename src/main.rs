mod auth;
mod cli;
mod doctor;
mod errors;
mod planner;
mod probe;
mod runner;

use anyhow::{bail, Context, Result};
use clap::Parser;
use dialoguer::Input;
use indicatif::ProgressBar;
use std::time::Duration;
use url::Url;

use crate::auth::{prompt_credentials, CredentialStore, Credentials, MacKeychainStore};
use crate::cli::{AuthArgs, Cli, Commands, DownloadArgs, ProtocolArg};
use crate::doctor::{find_aria2, run_doctor};
use crate::errors::AppError;
use crate::planner::build_transfer_plan;
use crate::probe::{
    probe_candidate_for_speed_test, resolve_candidates, select_candidate_with_probe, Protocol,
    SpeedProbeAttempt, SpeedProbeResult, UrlCandidate,
};
use crate::runner::{execute_aria2, looks_like_auth_error};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let code = match cli.command {
        Commands::Doctor => run_doctor()?,
        Commands::Auth { command } => run_auth(command)?,
        Commands::Download(args) => run_download(args)?,
        Commands::SpeedTest => run_speed_test()?,
    };

    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}

fn run_auth(command: AuthArgs) -> Result<i32> {
    let store = MacKeychainStore;

    match command {
        AuthArgs::Clear { host } => {
            store.clear(&host)?;
            println!("Cleared cached credentials for host: {host}");
            Ok(0)
        }
    }
}

fn run_download(args: DownloadArgs) -> Result<i32> {
    let aria2_path = find_aria2().ok_or(AppError::MissingAria2)?;

    let candidates = resolve_candidates(
        &args.url,
        args.protocol,
        args.http_url.as_deref(),
        args.ftp_url.as_deref(),
    )?;

    let selection = select_candidate_with_probe(&aria2_path, &candidates, args.protocol)?;

    println!("Protocol selection: {}", selection.reason);
    for probe in &selection.all_probes {
        println!(
            "- {:?}: {:.2} Mbps sampled over {:.1}s ({} bytes)",
            probe.protocol, probe.mbps, probe.sample_seconds, probe.sample_bytes
        );
    }

    let host = selection
        .chosen
        .url
        .host_str()
        .ok_or_else(|| AppError::MissingHost(selection.chosen.url.to_string()))?
        .to_string();

    let store = MacKeychainStore;

    let mut credentials = credentials_from_url(&selection.chosen.url);
    let mut from_keychain = false;

    if credentials.is_none() && !args.no_keychain {
        credentials = store.get(&host)?;
        from_keychain = credentials.is_some();
    }

    let mut plan = build_transfer_plan(
        selection.chosen.url.clone(),
        args.out,
        args.filename,
        args.max_connections,
        selection.best_probe.clone(),
        credentials.clone(),
    )?;

    println!("Download URL: {}", plan.chosen_url);
    println!("Output file: {}", plan.output_path().display());
    println!(
        "Connections: {} (split {})",
        plan.max_connections, plan.split
    );

    let mut aria_args = plan.aria2_args();

    if args.dry_run {
        println!("Dry-run command:");
        println!("aria2c {}", redact_sensitive_args(&aria_args).join(" "));
        return Ok(0);
    }

    let outcome = execute_aria2(&aria2_path, &aria_args)?;

    if outcome.success {
        maybe_store_credentials(&store, &host, &plan.credentials, args.no_keychain)?;
        println!("Download finished successfully.");
        return Ok(0);
    }

    // Retry once on auth-related failures.
    if looks_like_auth_error(&outcome.combined_log) {
        if from_keychain {
            eprintln!("Stored credentials were rejected; please enter fresh credentials.");
        } else {
            eprintln!("Authentication looks required; please enter credentials.");
        }

        let prompted = prompt_credentials(plan.credentials.as_ref().map(|c| c.username.as_str()))?;
        plan.credentials = Some(prompted.clone());
        aria_args = plan.aria2_args();

        let retry = execute_aria2(&aria2_path, &aria_args)?;
        if retry.success {
            maybe_store_credentials(&store, &host, &plan.credentials, args.no_keychain)?;
            println!("Download finished successfully.");
            return Ok(0);
        }

        bail!(
            "download failed after credential retry (exit code {:?})",
            retry.exit_code
        );
    }

    bail!("download failed (exit code {:?})", outcome.exit_code)
}

fn run_speed_test() -> Result<i32> {
    let aria2_path = find_aria2().ok_or(AppError::MissingAria2)?;
    let store = MacKeychainStore;
    const SAMPLE_SECONDS: u64 = 10;

    let http_base = Input::<String>::new()
        .with_prompt("HTTP base URL (e.g. https://files.example.com)")
        .interact_text()
        .context("HTTP base URL input failed")?;

    let ftp_base = Input::<String>::new()
        .with_prompt("FTP base URL (e.g. ftp://files.example.com)")
        .interact_text()
        .context("FTP base URL input failed")?;

    let remote_path = Input::<String>::new()
        .with_prompt("Remote path (e.g. movies/2026/sample.mkv)")
        .interact_text()
        .context("remote path input failed")?;

    let http_url = build_speed_test_url(&http_base, &remote_path, Protocol::Http)?;
    let ftp_url = build_speed_test_url(&ftp_base, &remote_path, Protocol::Ftp)?;

    println!("Starting speed test...");
    println!("- HTTP test URL: {}", redact_url_for_display(&http_url));
    println!("- FTP test URL: {}", redact_url_for_display(&ftp_url));

    let candidates = resolve_candidates(
        http_url.as_str(),
        ProtocolArg::Auto,
        None,
        Some(ftp_url.as_str()),
    )?;

    let mut attempts = Vec::new();

    for protocol in [Protocol::Http, Protocol::Ftp] {
        let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.protocol == protocol)
        else {
            continue;
        };

        let host = candidate
            .url
            .host_str()
            .ok_or_else(|| AppError::MissingHost(candidate.url.to_string()))?
            .to_string();

        let mut credentials = speed_test_credentials_for_candidate(&store, &candidate.url)?;
        let mut attempt =
            run_speed_probe_attempt(&aria2_path, candidate, credentials.as_ref(), SAMPLE_SECONDS);

        let should_retry_with_prompt = attempt.result.is_none()
            && attempt
                .error
                .as_deref()
                .map(probe_error_looks_like_auth)
                .unwrap_or(false);

        if should_retry_with_prompt {
            eprintln!(
                "{} probe appears to require valid credentials. Enter credentials to retry.",
                protocol_name(protocol)
            );

            let default_username = credentials
                .as_ref()
                .map(|creds| creds.username.as_str())
                .or_else(|| {
                    if candidate.url.username().is_empty() {
                        None
                    } else {
                        Some(candidate.url.username())
                    }
                });

            let prompted = prompt_credentials(default_username)?;
            credentials = Some(prompted);

            attempt = run_speed_probe_attempt(
                &aria2_path,
                candidate,
                credentials.as_ref(),
                SAMPLE_SECONDS,
            );

            if attempt.result.is_some() {
                maybe_store_credentials(&store, &host, &credentials, false)?;
            }
        }

        attempts.push(attempt);
    }

    println!("\nSpeed test report:");

    let http_attempt = attempts
        .iter()
        .find(|attempt| attempt.protocol == Protocol::Http);
    let ftp_attempt = attempts
        .iter()
        .find(|attempt| attempt.protocol == Protocol::Ftp);

    print_probe_line("HTTP", http_attempt);
    print_probe_line("FTP", ftp_attempt);
    print_probe_hint(http_attempt);
    print_probe_hint(ftp_attempt);

    let successful: Vec<SpeedProbeResult> = attempts
        .iter()
        .filter_map(|attempt| attempt.result.clone())
        .collect();

    print_download_time_estimates(&successful);

    if let Some((winner, runner_up)) = rank_probes(&successful) {
        if let Some(second) = runner_up {
            let lead = percent_lead(winner.mbps, second.mbps);
            println!(
                "Measured winner: {} ({:.2} Mbps, +{lead:.1}% vs {})",
                protocol_name(winner.protocol),
                winner.mbps,
                protocol_name(second.protocol)
            );
        } else {
            println!(
                "Measured winner: {} ({:.2} Mbps)",
                protocol_name(winner.protocol),
                winner.mbps
            );
        }

        println!(
            "Recommendation: use {} for this file path.",
            protocol_name(winner.protocol)
        );

        if successful.len() == 1 {
            println!("Only one protocol produced measurable throughput.");
        }
    } else {
        println!("No throughput sample could be collected from either protocol.");
        println!(
            "Tip: confirm both URLs are directly downloadable and include credentials if required."
        );
    }

    Ok(0)
}

fn print_download_time_estimates(successful: &[SpeedProbeResult]) {
    if successful.is_empty() {
        return;
    }

    println!("Estimated download time (if speed stays stable):");

    for protocol in [Protocol::Http, Protocol::Ftp] {
        let Some(probe) = successful.iter().find(|probe| probe.protocol == protocol) else {
            continue;
        };

        let speed_mbps = probe.mbps;
        let speed_mbs = speed_mbps / 8.0;
        let estimate_50 = estimate_download_duration_for_gb(50, speed_mbps)
            .map(format_duration_human)
            .unwrap_or_else(|| "n/a".to_string());
        let estimate_100 = estimate_download_duration_for_gb(100, speed_mbps)
            .map(format_duration_human)
            .unwrap_or_else(|| "n/a".to_string());

        println!(
            "- {} @ {:.2} Mbps ({:.2} MB/s): 50 GB ~ {}, 100 GB ~ {}",
            protocol_name(protocol),
            speed_mbps,
            speed_mbs,
            estimate_50,
            estimate_100
        );
    }
}

fn estimate_download_duration_for_gb(size_gb: u64, mbps: f64) -> Option<Duration> {
    if mbps <= 0.0 || !mbps.is_finite() {
        return None;
    }

    let total_bits = size_gb as f64 * 1_000_000_000.0 * 8.0;
    let bits_per_second = mbps * 1_000_000.0;
    let seconds = total_bits / bits_per_second;

    if seconds <= 0.0 || !seconds.is_finite() {
        return None;
    }

    Some(Duration::from_secs_f64(seconds))
}

fn format_duration_human(duration: Duration) -> String {
    let total_seconds = duration.as_secs().max(1);
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{}d {}h {}m", days, hours, minutes)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

fn format_bytes_human(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.2} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.2} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}

fn build_speed_test_url(base: &str, remote_path: &str, expected_protocol: Protocol) -> Result<Url> {
    let mut base_url = Url::parse(base).with_context(|| {
        format!(
            "invalid {} base URL: {base}",
            protocol_name(expected_protocol)
        )
    })?;

    let actual_protocol = Protocol::from_scheme(base_url.scheme());
    if actual_protocol != expected_protocol {
        match expected_protocol {
            Protocol::Http => bail!("HTTP base URL must use http or https"),
            Protocol::Ftp => bail!("FTP base URL must use ftp"),
            Protocol::Unknown => bail!("unsupported protocol"),
        }
    }

    let trimmed_remote_path = remote_path.trim().trim_start_matches('/');
    if trimmed_remote_path.is_empty() {
        bail!("remote path cannot be empty");
    }

    if !base_url.path().ends_with('/') {
        let mut path = base_url.path().to_string();
        path.push('/');
        base_url.set_path(&path);
    }

    base_url.join(trimmed_remote_path).with_context(|| {
        format!(
            "failed to combine {} base URL with remote path {trimmed_remote_path}",
            protocol_name(expected_protocol)
        )
    })
}

fn run_speed_probe_attempt(
    aria2_path: &str,
    candidate: &UrlCandidate,
    credentials: Option<&Credentials>,
    sample_seconds: u64,
) -> SpeedProbeAttempt {
    let resolved_candidate = match candidate_with_credentials(candidate, credentials) {
        Ok(candidate) => candidate,
        Err(err) => {
            return SpeedProbeAttempt {
                protocol: candidate.protocol,
                result: None,
                error: Some(err.to_string()),
            }
        }
    };

    let spinner = ProgressBar::new_spinner();
    spinner.set_message(format!(
        "Probing {} for ~{}s...",
        protocol_name(candidate.protocol),
        sample_seconds
    ));
    spinner.enable_steady_tick(Duration::from_millis(120));

    let result = probe_candidate_for_speed_test(aria2_path, &resolved_candidate, sample_seconds);

    match result {
        Ok(probe) => {
            spinner.finish_with_message(format!(
                "{} probe complete ({:.2} Mbps)",
                protocol_name(candidate.protocol),
                probe.mbps
            ));
            SpeedProbeAttempt {
                protocol: candidate.protocol,
                result: Some(probe),
                error: None,
            }
        }
        Err(err) => {
            spinner.finish_with_message(format!(
                "{} probe failed",
                protocol_name(candidate.protocol)
            ));
            SpeedProbeAttempt {
                protocol: candidate.protocol,
                result: None,
                error: Some(err.to_string()),
            }
        }
    }
}

fn speed_test_credentials_for_candidate(
    store: &MacKeychainStore,
    url: &Url,
) -> Result<Option<Credentials>> {
    let inline = credentials_from_url(url);
    let host = url
        .host_str()
        .ok_or_else(|| AppError::MissingHost(url.to_string()))?;
    let saved = store.get(host)?;

    Ok(match (inline, saved) {
        (Some(inline_creds), Some(saved_creds)) => {
            if inline_creds.password.is_empty()
                && (inline_creds.username.is_empty()
                    || inline_creds.username == saved_creds.username)
            {
                Some(saved_creds)
            } else {
                Some(inline_creds)
            }
        }
        (Some(inline_creds), None) => Some(inline_creds),
        (None, Some(saved_creds)) => Some(saved_creds),
        (None, None) => None,
    })
}

fn candidate_with_credentials(
    candidate: &UrlCandidate,
    credentials: Option<&Credentials>,
) -> Result<UrlCandidate> {
    let mut url = candidate.url.clone();

    if let Some(creds) = credentials {
        if url.set_username(&creds.username).is_err() {
            bail!("failed to set username on probe URL");
        }

        if url.set_password(Some(&creds.password)).is_err() {
            bail!("failed to set password on probe URL");
        }
    }

    Ok(UrlCandidate {
        url,
        protocol: candidate.protocol,
    })
}

fn probe_error_looks_like_auth(error: &str) -> bool {
    looks_like_auth_error(error) || error.contains("exit code 21") || error.contains("exit code 24")
}

fn redact_url_for_display(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_password(None);
    redacted.to_string()
}

fn print_probe_hint(attempt: Option<&SpeedProbeAttempt>) {
    let Some(SpeedProbeAttempt {
        protocol,
        result: None,
        error: Some(error),
    }) = attempt
    else {
        return;
    };

    if error.contains("exit code 3") {
        println!(
            "  {} hint: resource not found; verify remote path from this protocol's server root.",
            protocol_name(*protocol)
        );
    } else if error.contains("exit code 21") || error.contains("exit code 24") {
        println!(
            "  {} hint: authentication failed; verify username/password and protocol permissions.",
            protocol_name(*protocol)
        );
    }
}

fn print_probe_line(label: &str, attempt: Option<&SpeedProbeAttempt>) {
    match attempt {
        Some(SpeedProbeAttempt {
            result: Some(result),
            ..
        }) => println!(
            "- {label}: {:.2} Mbps ({:.2} MB/s), sampled {} over {:.1}s",
            result.mbps,
            result.mbps / 8.0,
            format_bytes_human(result.sample_bytes),
            result.sample_seconds
        ),
        Some(SpeedProbeAttempt {
            result: None,
            error: Some(error),
            ..
        }) => println!("- {label}: unavailable ({error})"),
        _ => println!("- {label}: unavailable"),
    }
}

fn rank_probes(
    probes: &[SpeedProbeResult],
) -> Option<(&SpeedProbeResult, Option<&SpeedProbeResult>)> {
    if probes.is_empty() {
        return None;
    }

    let mut sorted: Vec<&SpeedProbeResult> = probes.iter().collect();
    sorted.sort_by(|a, b| {
        b.mbps
            .partial_cmp(&a.mbps)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Some((sorted[0], sorted.get(1).copied()))
}

fn percent_lead(winner_mbps: f64, runner_up_mbps: f64) -> f64 {
    if runner_up_mbps <= 0.0 {
        return 100.0;
    }

    ((winner_mbps - runner_up_mbps) / runner_up_mbps * 100.0).max(0.0)
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Http => "HTTP",
        Protocol::Ftp => "FTP",
        Protocol::Unknown => "UNKNOWN",
    }
}

fn maybe_store_credentials(
    store: &MacKeychainStore,
    host: &str,
    credentials: &Option<Credentials>,
    no_keychain: bool,
) -> Result<()> {
    if no_keychain {
        return Ok(());
    }

    if let Some(creds) = credentials {
        store
            .set(host, creds)
            .with_context(|| format!("failed to cache credentials for host {host}"))?;
    }

    Ok(())
}

fn credentials_from_url(url: &Url) -> Option<Credentials> {
    if url.username().is_empty() {
        return None;
    }

    Some(Credentials {
        username: url.username().to_string(),
        password: url.password().unwrap_or_default().to_string(),
    })
}

fn redact_sensitive_args(args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| {
            if arg.starts_with("--http-passwd=") {
                "--http-passwd=***".to_string()
            } else if arg.starts_with("--ftp-passwd=") {
                "--ftp-passwd=***".to_string()
            } else {
                arg.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_speed_test_url_from_base_and_remote_path() {
        let url = build_speed_test_url(
            "https://files.example.com",
            "movies/2026/sample.mkv",
            Protocol::Http,
        )
        .expect("expected combined url");

        assert_eq!(
            url.as_str(),
            "https://files.example.com/movies/2026/sample.mkv"
        );
    }

    #[test]
    fn trims_leading_slash_from_remote_path() {
        let url = build_speed_test_url(
            "ftp://files.example.com/base",
            "/movies/2026/sample.mkv",
            Protocol::Ftp,
        )
        .expect("expected combined url");

        assert_eq!(
            url.as_str(),
            "ftp://files.example.com/base/movies/2026/sample.mkv"
        );
    }

    #[test]
    fn rejects_wrong_scheme_for_ftp_base() {
        let err = build_speed_test_url("https://files.example.com", "movie.mkv", Protocol::Ftp)
            .expect_err("expected ftp scheme validation error");

        assert!(err.to_string().contains("FTP base URL must use ftp"));
    }

    #[test]
    fn estimates_50gb_duration_at_100mbps() {
        let estimate = estimate_download_duration_for_gb(50, 100.0)
            .expect("expected a duration for positive speed");

        assert_eq!(estimate.as_secs(), 4_000);
    }

    #[test]
    fn formats_human_duration() {
        let formatted = format_duration_human(Duration::from_secs(4_000));

        assert_eq!(formatted, "1h 6m");
    }
}
