use anyhow::{bail, Context, Result};
use ftp::FtpStream;
use url::Url;

use crate::auth::Credentials;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RemoteListing {
    pub current_dir: String,
    pub entries: Vec<RemoteEntry>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RemoteDeleteKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RemoteDeleteSummary {
    pub target_path: String,
    pub deleted_files: u64,
    pub deleted_directories: u64,
    pub kind: RemoteDeleteKind,
}

pub fn list_ftp_directory(
    ftp_base_url: &str,
    remote_dir: &str,
    credentials: Option<&Credentials>,
) -> Result<RemoteListing> {
    let base_url = parse_ftp_base_url(ftp_base_url)?;
    let target_dir = combine_base_and_relative_path(base_url.path(), remote_dir);
    let mut client = connect_ftp(&base_url, credentials)?;

    client
        .cwd(&target_dir)
        .with_context(|| format!("failed to change remote directory to {target_dir}"))?;

    let list_lines = client.list(None).context("failed to list FTP directory")?;

    let mut entries: Vec<RemoteEntry> = list_lines
        .iter()
        .filter_map(|line| parse_ftp_list_line(line))
        .collect();

    if !list_lines.is_empty() && entries.is_empty() {
        if let Ok(names) = client.nlst(None) {
            entries = names
                .into_iter()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty() && name != "." && name != "..")
                .map(|name| RemoteEntry {
                    name,
                    is_dir: false,
                    size_bytes: None,
                })
                .collect();
        }
    }

    let _ = client.quit();

    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });

    Ok(RemoteListing {
        current_dir: normalize_remote_path(remote_dir),
        entries,
    })
}

pub fn delete_ftp_path(
    ftp_url: &str,
    credentials: Option<&Credentials>,
    recursive: bool,
) -> Result<RemoteDeleteSummary> {
    let url = parse_ftp_base_url(ftp_url)?;
    let target_path = combine_base_and_relative_path(url.path(), "");
    if target_path == "/" {
        bail!("refusing to delete FTP server root");
    }

    let mut client = connect_ftp(&url, credentials)?;
    let mut deleted_files = 0;
    let mut deleted_directories = 0;

    let kind = if recursive {
        delete_path_recursive(
            &mut client,
            &target_path,
            &mut deleted_files,
            &mut deleted_directories,
        )?
    } else {
        delete_path_non_recursive(
            &mut client,
            &target_path,
            &mut deleted_files,
            &mut deleted_directories,
        )?
    };

    let _ = client.quit();

    Ok(RemoteDeleteSummary {
        target_path,
        deleted_files,
        deleted_directories,
        kind,
    })
}

pub fn normalize_remote_path(value: &str) -> String {
    value
        .trim()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn join_remote_path(current_dir: &str, child_name: &str) -> String {
    let current = normalize_remote_path(current_dir);
    let child = child_name.trim().trim_matches('/');

    if child.is_empty() {
        return current;
    }

    if current.is_empty() {
        child.to_string()
    } else {
        format!("{current}/{child}")
    }
}

pub fn parent_remote_path(current_dir: &str) -> String {
    let normalized = normalize_remote_path(current_dir);
    if normalized.is_empty() {
        return String::new();
    }

    if normalized == ".." {
        return normalized;
    }

    let mut segments: Vec<&str> = normalized.split('/').collect();
    let _ = segments.pop();
    segments.join("/")
}

fn parse_ftp_base_url(value: &str) -> Result<Url> {
    let parsed = Url::parse(value).with_context(|| format!("invalid FTP base URL: {value}"))?;
    if parsed.scheme() != "ftp" {
        bail!("FTP base URL must use ftp");
    }
    Ok(parsed)
}

fn ftp_server_address(url: &Url) -> Result<String> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("FTP base URL must include host"))?;
    let port = url.port().unwrap_or(21);
    Ok(format!("{host}:{port}"))
}

fn connect_ftp(url: &Url, credentials: Option<&Credentials>) -> Result<FtpStream> {
    let address = ftp_server_address(url)?;
    let mut client = FtpStream::connect(address.as_str())
        .with_context(|| format!("failed to connect to {address}"))?;

    let (username, password) = resolve_login_credentials(url, credentials);
    client
        .login(&username, &password)
        .context("FTP login failed")?;

    Ok(client)
}

fn resolve_login_credentials(url: &Url, credentials: Option<&Credentials>) -> (String, String) {
    if let Some(creds) = credentials {
        return (creds.username.clone(), creds.password.clone());
    }

    if !url.username().is_empty() {
        return (
            url.username().to_string(),
            url.password().unwrap_or_default().to_string(),
        );
    }

    ("anonymous".to_string(), "anonymous@".to_string())
}

fn delete_path_non_recursive(
    client: &mut FtpStream,
    target_path: &str,
    deleted_files: &mut u64,
    deleted_directories: &mut u64,
) -> Result<RemoteDeleteKind> {
    if client.rm(target_path).is_ok() {
        *deleted_files += 1;
        return Ok(RemoteDeleteKind::File);
    }

    client.rmdir(target_path).with_context(|| {
        format!("failed to delete remote file or empty directory {target_path}")
    })?;
    *deleted_directories += 1;
    Ok(RemoteDeleteKind::Directory)
}

fn delete_path_recursive(
    client: &mut FtpStream,
    target_path: &str,
    deleted_files: &mut u64,
    deleted_directories: &mut u64,
) -> Result<RemoteDeleteKind> {
    if client.cwd(target_path).is_err() {
        client
            .rm(target_path)
            .with_context(|| format!("failed to delete remote file {target_path}"))?;
        *deleted_files += 1;
        return Ok(RemoteDeleteKind::File);
    }

    let entries = list_current_directory_entries(client)?;
    let _ = client.cwd("/");
    for entry in entries {
        let child_path = join_absolute_remote_path(target_path, &entry.name);
        if entry.is_dir {
            delete_path_recursive(client, &child_path, deleted_files, deleted_directories)?;
        } else {
            client
                .rm(&child_path)
                .with_context(|| format!("failed to delete remote file {child_path}"))?;
            *deleted_files += 1;
        }
    }

    client
        .rmdir(target_path)
        .with_context(|| format!("failed to delete remote directory {target_path}"))?;
    *deleted_directories += 1;
    Ok(RemoteDeleteKind::Directory)
}

fn list_current_directory_entries(client: &mut FtpStream) -> Result<Vec<RemoteEntry>> {
    let list_lines = client.list(None).context("failed to list FTP directory")?;
    let mut entries: Vec<RemoteEntry> = list_lines
        .iter()
        .filter_map(|line| parse_ftp_list_line(line))
        .collect();

    if !list_lines.is_empty() && entries.is_empty() {
        if let Ok(names) = client.nlst(None) {
            entries = names
                .into_iter()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty() && name != "." && name != "..")
                .map(|name| RemoteEntry {
                    name,
                    is_dir: false,
                    size_bytes: None,
                })
                .collect();
        }
    }

    Ok(entries)
}

pub fn combine_base_and_relative_path(base_path: &str, relative_path: &str) -> String {
    let mut segments = base_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<String>>();
    let minimum_depth = segments.len().saturating_sub(1);

    for segment in normalize_remote_path(relative_path)
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        if segment == "." {
            continue;
        }

        if segment == ".." {
            if segments.len() > minimum_depth {
                let _ = segments.pop();
            }
            continue;
        }

        segments.push(segment.to_string());
    }

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

fn join_absolute_remote_path(parent: &str, child_name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    let child = child_name.trim().trim_matches('/');

    if parent.is_empty() {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn parse_ftp_list_line(line: &str) -> Option<RemoteEntry> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(unix_entry) = parse_unix_list_line(trimmed) {
        return Some(unix_entry);
    }

    if let Some(windows_entry) = parse_windows_list_line(trimmed) {
        return Some(windows_entry);
    }

    if !trimmed.contains(' ') && trimmed != "." && trimmed != ".." {
        return Some(RemoteEntry {
            name: trimmed.to_string(),
            is_dir: false,
            size_bytes: None,
        });
    }

    None
}

fn parse_unix_list_line(line: &str) -> Option<RemoteEntry> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 9 {
        return None;
    }

    let permissions = tokens[0];
    let kind = permissions.chars().next()?;
    if !matches!(kind, 'd' | '-' | 'l') {
        return None;
    }

    let size_bytes = tokens.get(4).and_then(|value| value.parse::<u64>().ok());
    let mut name = tokens[8..].join(" ");

    if kind == 'l' {
        if let Some((left, _)) = name.split_once(" -> ") {
            name = left.to_string();
        }
    }

    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    Some(RemoteEntry {
        name,
        is_dir: kind == 'd',
        size_bytes,
    })
}

fn parse_windows_list_line(line: &str) -> Option<RemoteEntry> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 4 {
        return None;
    }

    let marker = tokens[2];
    let name = tokens[3..].join(" ");
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    if marker.eq_ignore_ascii_case("<DIR>") {
        return Some(RemoteEntry {
            name,
            is_dir: true,
            size_bytes: None,
        });
    }

    let compact_size = marker.replace(',', "");
    let size_bytes = compact_size.parse::<u64>().ok()?;
    Some(RemoteEntry {
        name,
        is_dir: false,
        size_bytes: Some(size_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_remote_path() {
        assert_eq!(normalize_remote_path("/movies//2026/"), "movies/2026");
    }

    #[test]
    fn joins_remote_path() {
        assert_eq!(
            join_remote_path("movies", "sample.mkv"),
            "movies/sample.mkv"
        );
    }

    #[test]
    fn computes_parent_remote_path() {
        assert_eq!(parent_remote_path("movies/2026"), "movies");
        assert_eq!(parent_remote_path("movies"), "");
        assert_eq!(parent_remote_path(""), "");
        assert_eq!(parent_remote_path(".."), "..");
    }

    #[test]
    fn combines_base_and_relative_directory() {
        assert_eq!(
            combine_base_and_relative_path("/base", "movies/2026"),
            "/base/movies/2026"
        );
    }

    #[test]
    fn combines_one_parent_above_base_directory() {
        assert_eq!(
            combine_base_and_relative_path("/downloads", "../incoming"),
            "/incoming"
        );
        assert_eq!(
            combine_base_and_relative_path("/base/downloads", "../../incoming"),
            "/base/incoming"
        );
    }

    #[test]
    fn combines_url_path_without_relative_path() {
        assert_eq!(
            combine_base_and_relative_path("/movies/sample.mkv", ""),
            "/movies/sample.mkv"
        );
    }

    #[test]
    fn joins_absolute_remote_path() {
        assert_eq!(
            join_absolute_remote_path("/movies/2026", "sample.mkv"),
            "/movies/2026/sample.mkv"
        );
        assert_eq!(join_absolute_remote_path("/", "sample.mkv"), "/sample.mkv");
    }

    #[test]
    fn parses_unix_file_entry() {
        let parsed = parse_ftp_list_line("-rw-r--r-- 1 user group 2048 Jan 10 12:00 sample.mkv")
            .expect("expected unix file entry");

        assert_eq!(parsed.name, "sample.mkv");
        assert!(!parsed.is_dir);
        assert_eq!(parsed.size_bytes, Some(2048));
    }

    #[test]
    fn parses_unix_directory_entry() {
        let parsed = parse_ftp_list_line("drwxr-xr-x 2 user group 4096 Jan 10 12:00 movies")
            .expect("expected unix directory entry");

        assert_eq!(parsed.name, "movies");
        assert!(parsed.is_dir);
    }

    #[test]
    fn parses_windows_directory_entry() {
        let parsed = parse_ftp_list_line("01-10-26  12:00PM       <DIR>          Movies")
            .expect("expected windows directory entry");

        assert_eq!(parsed.name, "Movies");
        assert!(parsed.is_dir);
    }
}
