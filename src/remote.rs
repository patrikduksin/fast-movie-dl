use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ftp::types::FileType;
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

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RemoteUploadSummary {
    pub local_path: PathBuf,
    pub target_path: String,
    pub bytes_uploaded: u64,
    pub files_uploaded: u64,
    pub directories_created: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RemoteUploadProgress {
    pub current_file: PathBuf,
    pub target_path: String,
    pub file_sent: u64,
    pub file_total: u64,
    pub total_sent: u64,
    pub total_bytes: u64,
    pub files_done: u64,
    pub files_total: u64,
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

pub fn upload_ftp_path<F>(
    local_path: &Path,
    ftp_url: &str,
    credentials: Option<&Credentials>,
    mut on_progress: F,
) -> Result<RemoteUploadSummary>
where
    F: FnMut(RemoteUploadProgress),
{
    let url = parse_ftp_base_url(ftp_url)?;
    let target_path = resolve_upload_target_path(local_path, url.path())?;
    let plan = build_upload_plan(local_path, &target_path)?;

    let mut client = connect_ftp(&url, credentials)?;
    client
        .transfer_type(FileType::Binary)
        .context("failed to set FTP binary transfer mode")?;

    let mut total_sent = 0;
    let mut files_done = 0;
    let mut directories_created = 0;

    for directory in &plan.directories {
        ensure_remote_directory(&mut client, directory)?;
        directories_created += 1;
    }

    for file_item in &plan.files {
        let file = File::open(&file_item.local_path).with_context(|| {
            format!(
                "failed to open local file {}",
                file_item.local_path.display()
            )
        })?;

        on_progress(RemoteUploadProgress {
            current_file: file_item.local_path.clone(),
            target_path: file_item.target_path.clone(),
            file_sent: 0,
            file_total: file_item.size_bytes,
            total_sent,
            total_bytes: plan.total_bytes,
            files_done,
            files_total: plan.files.len() as u64,
        });

        let mut reader = ProgressRead::new(file, file_item.size_bytes, |sent, total| {
            on_progress(RemoteUploadProgress {
                current_file: file_item.local_path.clone(),
                target_path: file_item.target_path.clone(),
                file_sent: sent,
                file_total: total,
                total_sent: total_sent + sent,
                total_bytes: plan.total_bytes,
                files_done,
                files_total: plan.files.len() as u64,
            });
        });

        client
            .put(&file_item.target_path, &mut reader)
            .with_context(|| {
                format!("failed to upload to remote path {}", file_item.target_path)
            })?;

        total_sent += file_item.size_bytes;
        files_done += 1;
    }

    let _ = client.quit();

    Ok(RemoteUploadSummary {
        local_path: local_path.to_path_buf(),
        target_path,
        bytes_uploaded: plan.total_bytes,
        files_uploaded: files_done,
        directories_created,
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

pub fn remote_upload_path_from_url(local_path: &Path, ftp_url: &str) -> Result<String> {
    let url = parse_ftp_base_url(ftp_url)?;
    resolve_upload_target_path(local_path, url.path())
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

fn resolve_upload_target_path(local_path: &Path, url_path: &str) -> Result<String> {
    let local_filename = local_path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("local upload path must include a file name"))?;

    if url_path.is_empty() || url_path.ends_with('/') {
        let base = combine_base_and_relative_path(url_path, local_filename);
        return Ok(base);
    }

    Ok(combine_base_and_relative_path(url_path, ""))
}

#[derive(Debug)]
struct UploadPlan {
    directories: Vec<String>,
    files: Vec<UploadFileItem>,
    total_bytes: u64,
}

#[derive(Debug)]
struct UploadFileItem {
    local_path: PathBuf,
    target_path: String,
    size_bytes: u64,
}

fn build_upload_plan(local_path: &Path, target_path: &str) -> Result<UploadPlan> {
    let metadata = local_path
        .metadata()
        .with_context(|| format!("failed to stat local upload path {}", local_path.display()))?;

    if metadata.is_file() {
        return Ok(UploadPlan {
            directories: parent_remote_directory(target_path).into_iter().collect(),
            files: vec![UploadFileItem {
                local_path: local_path.to_path_buf(),
                target_path: target_path.to_string(),
                size_bytes: metadata.len(),
            }],
            total_bytes: metadata.len(),
        });
    }

    if !metadata.is_dir() {
        bail!(
            "local upload path is not a file or directory: {}",
            local_path.display()
        );
    }

    let mut directories = vec![target_path.to_string()];
    let mut files = Vec::new();
    collect_directory_upload_items(
        local_path,
        local_path,
        target_path,
        &mut directories,
        &mut files,
    )?;

    directories.sort();
    directories.dedup();
    directories.sort_by_key(|path| path.matches('/').count());

    let total_bytes = files.iter().map(|item| item.size_bytes).sum();

    Ok(UploadPlan {
        directories,
        files,
        total_bytes,
    })
}

fn collect_directory_upload_items(
    root: &Path,
    current: &Path,
    target_root: &str,
    directories: &mut Vec<String>,
    files: &mut Vec<UploadFileItem>,
) -> Result<()> {
    let mut entries = fs::read_dir(current)
        .with_context(|| format!("failed to read directory {}", current.display()))?
        .collect::<std::result::Result<Vec<_>, io::Error>>()
        .with_context(|| format!("failed to read directory entry in {}", current.display()))?;

    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("failed to compute relative path for {}", path.display()))?;
        let remote_path = join_remote_relative_path(target_root, relative)?;

        if metadata.is_dir() {
            directories.push(remote_path.clone());
            collect_directory_upload_items(root, &path, target_root, directories, files)?;
        } else if metadata.is_file() {
            files.push(UploadFileItem {
                local_path: path,
                target_path: remote_path,
                size_bytes: metadata.len(),
            });
        }
    }

    Ok(())
}

fn join_remote_relative_path(target_root: &str, relative: &Path) -> Result<String> {
    let mut result = target_root.trim_end_matches('/').to_string();

    for component in relative.components() {
        let segment = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("upload path contains non-UTF-8 segment"))?;
        if segment.is_empty() {
            continue;
        }
        result.push('/');
        result.push_str(segment);
    }

    if result.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(result)
    }
}

fn parent_remote_directory(target_path: &str) -> Option<String> {
    let trimmed = target_path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;

    if parent.is_empty() {
        Some("/".to_string())
    } else {
        Some(parent.to_string())
    }
}

fn ensure_remote_directory(client: &mut FtpStream, target_path: &str) -> Result<()> {
    if target_path == "/" {
        return Ok(());
    }

    let mut current = String::new();
    for segment in target_path.split('/').filter(|segment| !segment.is_empty()) {
        current.push('/');
        current.push_str(segment);

        if client.cwd(&current).is_ok() {
            let _ = client.cwd("/");
            continue;
        }

        let _ = client.cwd("/");
        if client.mkdir(&current).is_err() && client.cwd(&current).is_err() {
            bail!("failed to create remote directory {current}");
        }
        let _ = client.cwd("/");
    }

    Ok(())
}

struct ProgressRead<R, F> {
    inner: R,
    total: u64,
    sent: u64,
    on_progress: F,
}

impl<R, F> ProgressRead<R, F> {
    fn new(inner: R, total: u64, on_progress: F) -> Self {
        Self {
            inner,
            total,
            sent: 0,
            on_progress,
        }
    }
}

impl<R, F> Read for ProgressRead<R, F>
where
    R: Read,
    F: FnMut(u64, u64),
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            self.sent += read as u64;
            (self.on_progress)(self.sent, self.total);
        }
        Ok(read)
    }
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
    fn resolves_upload_url_directory_to_local_filename() {
        let path = remote_upload_path_from_url(
            Path::new("/tmp/movie.mkv"),
            "ftp://files.example.com/uploads/",
        )
        .expect("expected upload path");

        assert_eq!(path, "/uploads/movie.mkv");
    }

    #[test]
    fn resolves_upload_url_file_as_exact_target() {
        let path = remote_upload_path_from_url(
            Path::new("/tmp/movie.mkv"),
            "ftp://files.example.com/uploads/custom.mkv",
        )
        .expect("expected upload path");

        assert_eq!(path, "/uploads/custom.mkv");
    }

    #[test]
    fn resolves_directory_upload_url_directory_to_local_directory_name() {
        let temp = tempfile::tempdir().expect("expected temp dir");
        let local_dir = temp.path().join("movies");
        std::fs::create_dir(&local_dir).expect("expected local dir");

        let path = remote_upload_path_from_url(&local_dir, "ftp://files.example.com/uploads/")
            .expect("expected upload path");

        assert_eq!(path, "/uploads/movies");
    }

    #[test]
    fn plans_nested_directory_upload() {
        let temp = tempfile::tempdir().expect("expected temp dir");
        let local_dir = temp.path().join("movies");
        let nested_dir = local_dir.join("set");
        std::fs::create_dir(&local_dir).expect("expected local dir");
        std::fs::create_dir(&nested_dir).expect("expected nested dir");
        std::fs::write(local_dir.join("a.mkv"), b"aaa").expect("expected file");
        std::fs::write(nested_dir.join("b.mkv"), b"bbbb").expect("expected nested file");

        let plan = build_upload_plan(&local_dir, "/uploads/movies").expect("expected upload plan");

        assert_eq!(plan.total_bytes, 7);
        assert_eq!(
            plan.directories,
            vec![
                "/uploads/movies".to_string(),
                "/uploads/movies/set".to_string()
            ]
        );
        assert_eq!(
            plan.files
                .iter()
                .map(|item| item.target_path.as_str())
                .collect::<Vec<_>>(),
            vec!["/uploads/movies/a.mkv", "/uploads/movies/set/b.mkv"]
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
