mod auth;
mod cli;
mod doctor;
mod errors;
mod planner;
mod probe;
mod runner;

use anyhow::{bail, Context, Result};
use clap::Parser;
use url::Url;

use crate::auth::{prompt_credentials, CredentialStore, Credentials, MacKeychainStore};
use crate::cli::{AuthArgs, Cli, Commands, DownloadArgs};
use crate::doctor::{find_aria2, run_doctor};
use crate::errors::AppError;
use crate::planner::build_transfer_plan;
use crate::probe::{resolve_candidates, select_candidate_with_probe};
use crate::runner::{execute_aria2, looks_like_auth_error};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let code = match cli.command {
        Commands::Doctor => run_doctor()?,
        Commands::Auth { command } => run_auth(command)?,
        Commands::Download(args) => run_download(args)?,
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
