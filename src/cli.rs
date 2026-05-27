use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "fast-movie-dl")]
#[command(about = "High-throughput movie downloader for HTTP/HTTPS/FTP using aria2c")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Launch terminal UI
    Tui,
    /// Download a large file with speed-optimized settings
    Download(DownloadArgs),
    /// Upload a local file to an FTP URL
    Upload(UploadArgs),
    /// Delete a remote FTP file or directory
    Delete(DeleteArgs),
    /// Run an interactive HTTP vs FTP speed test
    SpeedTest,
    /// Check local prerequisites
    Doctor,
    /// Manage saved credentials
    Auth {
        #[command(subcommand)]
        command: AuthArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthArgs {
    /// Remove cached credentials for a host
    Clear {
        /// Hostname (example: files.example.com)
        #[arg(long)]
        host: String,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum ProtocolArg {
    Auto,
    Http,
    Ftp,
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    /// Primary URL to download
    pub url: String,

    /// Output directory OR full output file path
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Override output file name
    #[arg(long)]
    pub filename: Option<String>,

    /// Protocol strategy
    #[arg(long, value_enum, default_value_t = ProtocolArg::Auto)]
    pub protocol: ProtocolArg,

    /// Optional explicit HTTP/HTTPS URL for protocol comparison
    #[arg(long)]
    pub http_url: Option<String>,

    /// Optional explicit FTP URL for protocol comparison
    #[arg(long)]
    pub ftp_url: Option<String>,

    /// Do not read/write credentials in macOS Keychain
    #[arg(long)]
    pub no_keychain: bool,

    /// Override max parallel server connections
    #[arg(long)]
    pub max_connections: Option<u32>,

    /// Print chosen plan and aria2 command without starting download
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct UploadArgs {
    /// Local file to upload
    pub local_path: PathBuf,

    /// FTP target URL. End with / to upload using the local file name.
    pub url: String,

    /// Do not read/write credentials in macOS Keychain
    #[arg(long)]
    pub no_keychain: bool,

    /// Print the resolved target without uploading anything
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct DeleteArgs {
    /// FTP URL for the remote file or directory to delete
    pub url: String,

    /// Recursively delete directory contents before deleting the directory
    #[arg(long)]
    pub recursive: bool,

    /// Confirm deletion without prompting
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Do not read credentials from macOS Keychain
    #[arg(long)]
    pub no_keychain: bool,

    /// Print the resolved target without deleting anything
    #[arg(long)]
    pub dry_run: bool,
}
