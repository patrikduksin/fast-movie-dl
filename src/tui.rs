use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use url::Url;

use crate::auth::{CredentialStore, Credentials, MacKeychainStore};
use crate::cli::ProtocolArg;
use crate::config::{AppConfig, MachineProfile};
use crate::doctor::find_aria2;
use crate::errors::AppError;
use crate::planner::build_transfer_plan;
use crate::probe::{
    resolve_candidates, select_candidate_with_probe, Protocol, SpeedProbeResult, UrlCandidate,
};
use crate::remote::{
    combine_base_and_relative_path, join_remote_path, list_ftp_directory, normalize_remote_path,
    parent_remote_path, RemoteEntry, RemoteListing,
};
use crate::runner::{execute_aria2_capture_with_sink, looks_like_auth_error, RunOutcome};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Screen {
    Profiles,
    Browser,
    Form,
    Running,
    Result,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FormField {
    ProfileName,
    HttpBaseUrl,
    FtpBaseUrl,
    RemotePath,
    OutputDir,
    Filename,
    Username,
    Password,
    RememberKeychain,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum InputMode {
    Normal,
    Insert,
}

impl InputMode {
    fn label(self) -> &'static str {
        match self {
            InputMode::Normal => "NORMAL",
            InputMode::Insert => "INSERT",
        }
    }
}

impl FormField {
    fn all() -> &'static [FormField] {
        &[
            FormField::ProfileName,
            FormField::HttpBaseUrl,
            FormField::FtpBaseUrl,
            FormField::RemotePath,
            FormField::OutputDir,
            FormField::Filename,
            FormField::Username,
            FormField::Password,
            FormField::RememberKeychain,
        ]
    }

    fn next(self) -> Self {
        let items = Self::all();
        let index = items.iter().position(|item| *item == self).unwrap_or(0);
        items[(index + 1) % items.len()]
    }

    fn previous(self) -> Self {
        let items = Self::all();
        let index = items.iter().position(|item| *item == self).unwrap_or(0);
        items[(index + items.len() - 1) % items.len()]
    }
}

#[derive(Debug, Clone)]
struct FormState {
    profile_name: String,
    http_base_url: String,
    ftp_base_url: String,
    remote_path: String,
    output_dir: String,
    filename: String,
    username: String,
    password: String,
    remember_keychain: bool,
}

impl FormState {
    fn from_profile(profile: Option<&MachineProfile>) -> Self {
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string();

        match profile {
            Some(item) => Self {
                profile_name: item.name.clone(),
                http_base_url: item.http_base_url.clone(),
                ftp_base_url: item.ftp_base_url.clone(),
                remote_path: String::new(),
                output_dir: item
                    .output_dir
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or(cwd),
                filename: String::new(),
                username: String::new(),
                password: String::new(),
                remember_keychain: true,
            },
            None => Self {
                profile_name: String::new(),
                http_base_url: String::new(),
                ftp_base_url: String::new(),
                remote_path: String::new(),
                output_dir: cwd,
                filename: String::new(),
                username: String::new(),
                password: String::new(),
                remember_keychain: true,
            },
        }
    }

    fn save_profile(&self, config: &mut AppConfig) -> Result<()> {
        let name = self.profile_name.trim();
        if name.is_empty() {
            bail!("profile name cannot be empty");
        }

        let _ = parse_base_url(&self.http_base_url, Protocol::Http)?;
        let _ = parse_base_url(&self.ftp_base_url, Protocol::Ftp)?;

        let output_dir = if self.output_dir.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.output_dir.trim()))
        };

        let existing_last_remote_dir = config
            .profiles
            .iter()
            .find(|item| item.name == name)
            .and_then(|item| item.last_remote_dir.clone());

        config.upsert_profile(MachineProfile {
            name: name.to_string(),
            http_base_url: self.http_base_url.trim().to_string(),
            ftp_base_url: self.ftp_base_url.trim().to_string(),
            output_dir,
            last_remote_dir: existing_last_remote_dir,
        });
        config.save()?;

        Ok(())
    }

    fn download_input(&self) -> Result<DownloadInput> {
        let http_base_url = self.http_base_url.trim();
        let ftp_base_url = self.ftp_base_url.trim();
        let remote_path = self.remote_path.trim();

        let _ = parse_base_url(http_base_url, Protocol::Http)?;
        let _ = parse_base_url(ftp_base_url, Protocol::Ftp)?;

        if remote_path.is_empty() {
            bail!("remote path cannot be empty");
        }

        let output_dir = if self.output_dir.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.output_dir.trim()))
        };

        let filename = if self.filename.trim().is_empty() {
            None
        } else {
            Some(self.filename.trim().to_string())
        };

        let credentials = if self.username.trim().is_empty() {
            None
        } else {
            Some(Credentials {
                username: self.username.trim().to_string(),
                password: self.password.clone(),
            })
        };

        Ok(DownloadInput {
            http_base_url: http_base_url.to_string(),
            ftp_base_url: ftp_base_url.to_string(),
            remote_path: remote_path.to_string(),
            output_dir,
            filename,
            credentials,
            remember_keychain: self.remember_keychain,
        })
    }
}

#[derive(Debug, Clone)]
struct DownloadInput {
    http_base_url: String,
    ftp_base_url: String,
    remote_path: String,
    output_dir: Option<PathBuf>,
    filename: Option<String>,
    credentials: Option<Credentials>,
    remember_keychain: bool,
}

#[derive(Debug)]
struct DownloadSummary {
    reason: String,
    chosen_url: String,
    output_path: String,
    probes: Vec<SpeedProbeResult>,
    outcome: RunOutcome,
    auth_hint: bool,
    log_path: String,
}

#[derive(Debug)]
enum DownloadWorkerEvent {
    Status(String),
    LogPath(String),
    LogLine(String),
    Finished(Result<DownloadSummary>),
}

#[derive(Debug, Clone)]
struct BrowserListRequest {
    ftp_base_url: String,
    remote_dir: String,
    credentials: Option<Credentials>,
}

#[derive(Debug, Clone)]
struct BrowserListSummary {
    listing: RemoteListing,
}

struct TuiApp {
    config: AppConfig,
    screen: Screen,
    selected_profile: usize,
    form: FormState,
    active_field: FormField,
    input_mode: InputMode,
    active_profile_name: Option<String>,
    browser_current_dir: String,
    browser_selected: usize,
    browser_entries: Vec<RemoteEntry>,
    browser_error: Option<String>,
    listing_worker: Option<Receiver<Result<BrowserListSummary>>>,
    status: Option<String>,
    worker: Option<Receiver<DownloadWorkerEvent>>,
    running_log_lines: Vec<String>,
    running_log_path: Option<String>,
    tick_count: usize,
    result: Option<DownloadSummary>,
    result_error: Option<String>,
    should_quit: bool,
}

impl TuiApp {
    fn new(config: AppConfig) -> Self {
        Self {
            config,
            screen: Screen::Profiles,
            selected_profile: 0,
            form: FormState::from_profile(None),
            active_field: FormField::ProfileName,
            input_mode: InputMode::Normal,
            active_profile_name: None,
            browser_current_dir: String::new(),
            browser_selected: 0,
            browser_entries: Vec::new(),
            browser_error: None,
            listing_worker: None,
            status: None,
            worker: None,
            running_log_lines: Vec::new(),
            running_log_path: None,
            tick_count: 0,
            result: None,
            result_error: None,
            should_quit: false,
        }
    }

    fn on_tick(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
    }

    fn poll_worker(&mut self) {
        self.poll_listing_worker();
        self.poll_download_worker();
    }

    fn open_selected_profile(&mut self) {
        if let Some(profile) = self.config.profiles.get(self.selected_profile).cloned() {
            self.form = FormState::from_profile(Some(&profile));
            self.hydrate_form_credentials_from_keychain();
            self.active_profile_name = Some(profile.name.clone());
            self.browser_current_dir =
                normalize_remote_path(profile.last_remote_dir.as_deref().unwrap_or_default());
            self.browser_selected = 0;
            self.browser_entries.clear();
            self.browser_error = None;
            self.active_field = FormField::RemotePath;
            self.input_mode = InputMode::Normal;
            self.screen = Screen::Browser;
            self.status = Some(format!("Loaded profile: {}", profile.name));
            self.reload_browser_listing();
        } else {
            self.form = FormState::from_profile(None);
            self.active_profile_name = None;
            self.active_field = FormField::ProfileName;
            self.input_mode = InputMode::Normal;
            self.screen = Screen::Form;
            self.status = Some("Create a new machine profile".to_string());
        }
    }

    fn new_profile(&mut self) {
        self.form = FormState::from_profile(None);
        self.active_profile_name = None;
        self.active_field = FormField::ProfileName;
        self.input_mode = InputMode::Normal;
        self.screen = Screen::Form;
        self.status = Some("New profile".to_string());
    }

    fn poll_listing_worker(&mut self) {
        let Some(worker) = &self.listing_worker else {
            return;
        };

        match worker.try_recv() {
            Ok(result) => {
                self.listing_worker = None;
                match result {
                    Ok(summary) => {
                        self.browser_current_dir = summary.listing.current_dir;
                        self.browser_entries = summary.listing.entries;
                        if self.browser_selected >= self.browser_entries.len() {
                            self.browser_selected = self.browser_entries.len().saturating_sub(1);
                        }
                        self.browser_error = None;
                        self.status = Some(format!(
                            "Loaded {} entries in {}",
                            self.browser_entries.len(),
                            display_remote_dir(&self.browser_current_dir)
                        ));
                    }
                    Err(err) => {
                        self.browser_entries.clear();
                        self.browser_selected = 0;
                        self.browser_error = Some(err.to_string());
                        self.status = Some("Directory listing failed".to_string());
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.listing_worker = None;
                self.browser_entries.clear();
                self.browser_selected = 0;
                self.browser_error = Some("directory listing worker disconnected".to_string());
                self.status = Some("Directory listing failed".to_string());
            }
        }
    }

    fn poll_download_worker(&mut self) {
        let mut clear_worker = false;

        while let Some(worker) = self.worker.as_ref() {
            let event = worker.try_recv();
            match event {
                Ok(DownloadWorkerEvent::Status(message)) => {
                    self.status = Some(message);
                }
                Ok(DownloadWorkerEvent::LogPath(path)) => {
                    self.running_log_path = Some(path);
                }
                Ok(DownloadWorkerEvent::LogLine(line)) => {
                    const MAX_RUNNING_LINES: usize = 160;
                    self.running_log_lines.push(line);
                    if self.running_log_lines.len() > MAX_RUNNING_LINES {
                        let overflow = self.running_log_lines.len() - MAX_RUNNING_LINES;
                        self.running_log_lines.drain(..overflow);
                    }
                }
                Ok(DownloadWorkerEvent::Finished(result)) => {
                    clear_worker = true;
                    match result {
                        Ok(summary) => {
                            self.running_log_path = Some(summary.log_path.clone());
                            self.result = Some(summary);
                            self.result_error = None;
                        }
                        Err(err) => {
                            self.result = None;
                            self.result_error = Some(err.to_string());
                        }
                    }
                    self.screen = Screen::Result;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    clear_worker = true;
                    self.result = None;
                    self.result_error =
                        Some("download worker unexpectedly disconnected".to_string());
                    self.screen = Screen::Result;
                    break;
                }
            }
        }

        if clear_worker {
            self.worker = None;
        }
    }

    fn hydrate_form_credentials_from_keychain(&mut self) {
        if !self.form.username.trim().is_empty() {
            return;
        }

        let Ok(url) = Url::parse(self.form.ftp_base_url.trim()) else {
            return;
        };
        let Some(host) = url.host_str() else {
            return;
        };

        let store = MacKeychainStore;
        if let Ok(Some(creds)) = store.get(host) {
            self.form.username = creds.username;
            self.form.password = creds.password;
        }
    }

    fn reload_browser_listing(&mut self) {
        if self.listing_worker.is_some() {
            return;
        }

        let request = BrowserListRequest {
            ftp_base_url: self.form.ftp_base_url.clone(),
            remote_dir: self.browser_current_dir.clone(),
            credentials: form_credentials(&self.form),
        };

        let (tx, rx) = mpsc::channel();
        self.listing_worker = Some(rx);
        self.browser_error = None;
        self.status = Some(format!(
            "Loading directory {}...",
            display_remote_dir(&self.browser_current_dir)
        ));

        thread::spawn(move || {
            let result = run_browser_list_job(request);
            let _ = tx.send(result);
        });
    }

    fn persist_last_remote_dir(&mut self) {
        let Some(profile_name) = &self.active_profile_name else {
            return;
        };

        let Some(profile) = self
            .config
            .profiles
            .iter_mut()
            .find(|item| item.name == *profile_name)
        else {
            return;
        };

        profile.last_remote_dir = if self.browser_current_dir.is_empty() {
            None
        } else {
            Some(self.browser_current_dir.clone())
        };

        if let Err(err) = self.config.save() {
            self.status = Some(format!("Failed to save profile: {err}"));
        }
    }

    fn delete_selected_profile(&mut self) {
        if self.config.profiles.is_empty() {
            self.status = Some("No profile to delete".to_string());
            return;
        }

        let removed_name = self
            .config
            .profiles
            .get(self.selected_profile)
            .map(|item| item.name.clone())
            .unwrap_or_else(|| "profile".to_string());

        self.config.delete_profile_by_index(self.selected_profile);
        if self.active_profile_name.as_deref() == Some(removed_name.as_str()) {
            self.active_profile_name = None;
        }
        if self.selected_profile >= self.config.profiles.len() && self.selected_profile > 0 {
            self.selected_profile -= 1;
        }

        match self.config.save() {
            Ok(()) => {
                self.status = Some(format!("Deleted profile: {removed_name}"));
            }
            Err(err) => {
                self.status = Some(format!("Failed to save config: {err}"));
            }
        }
    }

    fn save_profile(&mut self) {
        match self.form.save_profile(&mut self.config) {
            Ok(()) => {
                self.status = Some("Profile saved".to_string());
                let profile_name = self.form.profile_name.trim().to_string();
                if !profile_name.is_empty() {
                    self.active_profile_name = Some(profile_name.clone());
                }
                if let Some(index) = self
                    .config
                    .profiles
                    .iter()
                    .position(|item| item.name == profile_name)
                {
                    self.selected_profile = index;
                }
            }
            Err(err) => {
                self.status = Some(format!("Failed to save profile: {err}"));
            }
        }
    }

    fn start_download(&mut self) {
        if self.worker.is_some() {
            return;
        }

        let input = match self.form.download_input() {
            Ok(input) => input,
            Err(err) => {
                self.status = Some(format!("Invalid input: {err}"));
                return;
            }
        };

        let (tx, rx) = mpsc::channel();
        self.worker = Some(rx);
        self.screen = Screen::Running;
        self.result = None;
        self.result_error = None;
        self.running_log_lines.clear();
        self.running_log_path = None;
        self.status = Some("Download started".to_string());

        thread::spawn(move || {
            let result = run_download_job(input, &tx);
            let _ = tx.send(DownloadWorkerEvent::Finished(result));
        });
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        match self.screen {
            Screen::Profiles => self.handle_profiles_key(key),
            Screen::Browser => self.handle_browser_key(key),
            Screen::Form => self.handle_form_key(key),
            Screen::Running => self.handle_running_key(key),
            Screen::Result => self.handle_result_key(key),
        }
    }

    fn handle_profiles_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.config.profiles.is_empty() {
                    self.selected_profile =
                        (self.selected_profile + 1).min(self.config.profiles.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_profile > 0 {
                    self.selected_profile -= 1;
                }
            }
            KeyCode::Enter => self.open_selected_profile(),
            KeyCode::Char('n') => self.new_profile(),
            KeyCode::Char('d') => self.delete_selected_profile(),
            _ => {}
        }
    }

    fn handle_browser_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = Screen::Profiles;
                self.status = Some("Back to profiles".to_string());
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.browser_entries.is_empty() {
                    self.browser_selected =
                        (self.browser_selected + 1).min(self.browser_entries.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.browser_selected > 0 {
                    self.browser_selected -= 1;
                }
            }
            KeyCode::Char('g') => {
                self.browser_selected = 0;
            }
            KeyCode::Char('G') => {
                if !self.browser_entries.is_empty() {
                    self.browser_selected = self.browser_entries.len() - 1;
                }
            }
            KeyCode::Char('h') | KeyCode::Backspace => {
                let parent =
                    browser_parent_remote_path(&self.browser_current_dir, &self.form.ftp_base_url);
                if parent != self.browser_current_dir {
                    self.browser_current_dir = parent;
                    self.browser_selected = 0;
                    self.persist_last_remote_dir();
                    self.reload_browser_listing();
                }
            }
            KeyCode::Char('r') => self.reload_browser_listing(),
            KeyCode::Char('e') => {
                self.screen = Screen::Form;
                self.input_mode = InputMode::Normal;
                self.active_field = FormField::RemotePath;
                self.status = Some("Edit download inputs".to_string());
            }
            KeyCode::Enter | KeyCode::Char('l') => {
                if self.listing_worker.is_some() {
                    return;
                }

                let Some(selected) = self.browser_entries.get(self.browser_selected).cloned()
                else {
                    return;
                };

                let selected_path = join_remote_path(&self.browser_current_dir, &selected.name);
                if selected.is_dir {
                    self.browser_current_dir = selected_path;
                    self.browser_selected = 0;
                    self.persist_last_remote_dir();
                    self.reload_browser_listing();
                } else {
                    self.form.remote_path = selected_path;
                    self.status = Some(format!(
                        "Selected file {}. Starting probe + download...",
                        self.form.remote_path
                    ));
                    self.start_download();
                }
            }
            _ => {}
        }
    }

    fn handle_form_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.save_profile();
            return;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_form_key_normal(key),
            InputMode::Insert => self.handle_form_key_insert(key),
        }
    }

    fn handle_form_key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.screen = if self.active_profile_name.is_some() {
                    Screen::Browser
                } else {
                    Screen::Profiles
                };
                self.input_mode = InputMode::Normal;
                self.status = Some("Back".to_string());
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('l') => {
                self.active_field = self.active_field.next();
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('h') => {
                self.active_field = self.active_field.previous();
            }
            KeyCode::F(2) | KeyCode::Char('w') => self.save_profile(),
            KeyCode::F(5) | KeyCode::Char('r') => self.start_download(),
            KeyCode::Char('b') => {
                self.screen = Screen::Browser;
                self.input_mode = InputMode::Normal;
                self.browser_current_dir = if self.form.remote_path.trim().is_empty() {
                    self.browser_current_dir.clone()
                } else {
                    parent_remote_path(&self.form.remote_path)
                };
                self.persist_last_remote_dir();
                self.reload_browser_listing();
            }
            KeyCode::Char('i') | KeyCode::Char('a') => {
                if self.active_field == FormField::RememberKeychain {
                    self.form.remember_keychain = !self.form.remember_keychain;
                } else {
                    self.input_mode = InputMode::Insert;
                    self.status = Some("INSERT mode (Esc to return to NORMAL)".to_string());
                }
            }
            KeyCode::Enter => {
                if self.active_field == FormField::RememberKeychain {
                    self.form.remember_keychain = !self.form.remember_keychain;
                } else {
                    self.input_mode = InputMode::Insert;
                    self.status = Some("INSERT mode (Esc to return to NORMAL)".to_string());
                }
            }
            KeyCode::Char(' ') if self.active_field == FormField::RememberKeychain => {
                self.form.remember_keychain = !self.form.remember_keychain;
            }
            _ => {}
        }
    }

    fn handle_form_key_insert(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.status = Some("NORMAL mode".to_string());
            }
            KeyCode::Tab | KeyCode::Down => {
                self.active_field = self.active_field.next();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active_field = self.active_field.previous();
            }
            KeyCode::Enter => {
                if self.active_field == FormField::RememberKeychain {
                    self.form.remember_keychain = !self.form.remember_keychain;
                }
            }
            KeyCode::Backspace => {
                if let Some(text) = self.current_field_text_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(' ') if self.active_field == FormField::RememberKeychain => {
                self.form.remember_keychain = !self.form.remember_keychain;
            }
            KeyCode::Char(ch) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    if let Some(text) = self.current_field_text_mut() {
                        text.push(ch);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_running_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
            self.status = Some("Download is running. Wait for completion.".to_string());
        }
    }

    fn handle_result_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('p') => self.screen = Screen::Profiles,
            KeyCode::Char('e') | KeyCode::Enter => self.screen = Screen::Form,
            KeyCode::Char('r') => self.start_download(),
            _ => {}
        }
    }

    fn current_field_text_mut(&mut self) -> Option<&mut String> {
        match self.active_field {
            FormField::ProfileName => Some(&mut self.form.profile_name),
            FormField::HttpBaseUrl => Some(&mut self.form.http_base_url),
            FormField::FtpBaseUrl => Some(&mut self.form.ftp_base_url),
            FormField::RemotePath => Some(&mut self.form.remote_path),
            FormField::OutputDir => Some(&mut self.form.output_dir),
            FormField::Filename => Some(&mut self.form.filename),
            FormField::Username => Some(&mut self.form.username),
            FormField::Password => Some(&mut self.form.password),
            FormField::RememberKeychain => None,
        }
    }

    fn render(&self, frame: &mut ratatui::Frame<'_>) {
        match self.screen {
            Screen::Profiles => self.render_profiles(frame),
            Screen::Browser => self.render_browser(frame),
            Screen::Form => self.render_form(frame),
            Screen::Running => self.render_running(frame),
            Screen::Result => self.render_result(frame),
        }
    }

    fn render_profiles(&self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(frame.area());

        let title = Paragraph::new("fast-movie-dl TUI - Profiles")
            .block(Block::default().borders(Borders::ALL).title("Home"));
        frame.render_widget(title, chunks[0]);

        let mut items: Vec<ListItem<'_>> = self
            .config
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let marker = if index == self.selected_profile {
                    "> "
                } else {
                    "  "
                };
                ListItem::new(Line::from(vec![Span::raw(format!(
                    "{marker}{}",
                    profile.name
                ))]))
            })
            .collect();

        if items.is_empty() {
            items.push(ListItem::new(Line::from("  (no saved profiles)")));
        }

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Saved Configs"),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));
        frame.render_widget(list, chunks[1]);

        let status_line = self.status.as_deref().unwrap_or(
            "Enter=browse profile, n=new/edit profile, d=delete, q=quit. Profiles store machine URLs and default output dir.",
        );
        let footer = Paragraph::new(status_line)
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, chunks[2]);
    }

    fn render_browser(&self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(frame.area());

        let title = Paragraph::new("Browse remote FTP directory and pick a file").block(
            Block::default()
                .borders(Borders::ALL)
                .title("Remote Browser"),
        );
        frame.render_widget(title, chunks[0]);

        let listing_state = if self.listing_worker.is_some() {
            format!(
                "Loading {}...",
                display_remote_dir(&self.browser_current_dir)
            )
        } else if let Some(error) = &self.browser_error {
            format!("Error: {error}")
        } else {
            format!(
                "Directory: {} ({} entries)",
                display_remote_dir(&self.browser_current_dir),
                self.browser_entries.len()
            )
        };

        let status = Paragraph::new(listing_state)
            .block(Block::default().borders(Borders::ALL).title("Status"))
            .wrap(Wrap { trim: true });
        frame.render_widget(status, chunks[1]);

        let mut items: Vec<ListItem<'_>> = self
            .browser_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let marker = if index == self.browser_selected {
                    "> "
                } else {
                    "  "
                };
                let kind = if entry.is_dir { "[D]" } else { "[F]" };
                let size = entry
                    .size_bytes
                    .map(format_bytes_human)
                    .unwrap_or_else(|| "-".to_string());
                let name = if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                };
                ListItem::new(Line::from(format!("{marker}{kind} {name}  ({size})")))
            })
            .collect();

        if items.is_empty() {
            let placeholder = if self.listing_worker.is_some() {
                "  loading..."
            } else {
                "  (directory is empty)"
            };
            items.push(ListItem::new(Line::from(placeholder)));
        }

        let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Entries"));
        frame.render_widget(list, chunks[2]);

        let footer = Paragraph::new(
            self.status.as_deref().unwrap_or(
                "j/k move, Enter open dir or download file, h/backspace parent, r refresh, e edit, q back.",
            ),
        )
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .wrap(Wrap { trim: true });
        frame.render_widget(footer, chunks[3]);
    }

    fn render_form(&self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(4),
            ])
            .split(frame.area());

        let title = Paragraph::new("Edit machine settings and start download").block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Download Setup [{}]", self.input_mode.label())),
        );
        frame.render_widget(title, chunks[0]);

        let password_mask = if self.form.password.is_empty() {
            String::new()
        } else {
            "*".repeat(self.form.password.chars().count())
        };

        let remember_value = if self.form.remember_keychain {
            "[x] yes"
        } else {
            "[ ] no"
        };

        let rows = vec![
            (
                FormField::ProfileName,
                "Profile name",
                self.form.profile_name.as_str(),
            ),
            (
                FormField::HttpBaseUrl,
                "HTTP base URL",
                self.form.http_base_url.as_str(),
            ),
            (
                FormField::FtpBaseUrl,
                "FTP base URL",
                self.form.ftp_base_url.as_str(),
            ),
            (
                FormField::RemotePath,
                "Remote path",
                self.form.remote_path.as_str(),
            ),
            (
                FormField::OutputDir,
                "Output directory",
                self.form.output_dir.as_str(),
            ),
            (
                FormField::Filename,
                "Filename override",
                self.form.filename.as_str(),
            ),
            (
                FormField::Username,
                "Username (optional)",
                self.form.username.as_str(),
            ),
            (FormField::Password, "Password (optional)", &password_mask),
            (
                FormField::RememberKeychain,
                "Save creds to Keychain",
                remember_value,
            ),
        ];

        let lines: Vec<ListItem<'_>> = rows
            .into_iter()
            .map(|(field, label, value)| {
                let style = if field == self.active_field {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{label:<24}"), style),
                    Span::raw(" : "),
                    Span::styled(value.to_string(), style),
                ]))
            })
            .collect();

        let list = List::new(lines).block(Block::default().borders(Borders::ALL).title("Fields"));
        frame.render_widget(list, chunks[1]);

        let status_line = self.status.as_deref().unwrap_or(match self.input_mode {
            InputMode::Normal => {
                "NORMAL: i insert, hjkl move fields, w save, b browse, r run, q back, Enter toggles checkbox/edits field."
            }
            InputMode::Insert => {
                "INSERT: type to edit, Backspace delete, Tab move field, Esc to NORMAL."
            }
        });
        let footer = Paragraph::new(status_line)
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .wrap(Wrap { trim: true });
        frame.render_widget(footer, chunks[2]);
    }

    fn render_running(&self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(5),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(frame.area());

        let spinner_frames = ["|", "/", "-", "\\"];
        let spinner = spinner_frames[self.tick_count % spinner_frames.len()];
        let phase = self.status.as_deref().unwrap_or("Working...");

        let title = Paragraph::new("Download in progress")
            .block(Block::default().borders(Borders::ALL).title("Running"));
        frame.render_widget(title, chunks[0]);

        let body = Paragraph::new(format!(
            "{spinner} {phase}\nRemote path: {}\nOutput dir: {}",
            self.form.remote_path,
            if self.form.output_dir.trim().is_empty() {
                "(current directory)"
            } else {
                self.form.output_dir.trim()
            }
        ))
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .wrap(Wrap { trim: true });
        frame.render_widget(body, chunks[1]);

        let log_lines: Vec<Line<'_>> = if self.running_log_lines.is_empty() {
            vec![Line::from("(waiting for aria2 output)")]
        } else {
            tail_vec(&self.running_log_lines, 20)
                .into_iter()
                .map(Line::from)
                .collect()
        };

        let logs = Paragraph::new(log_lines)
            .block(Block::default().borders(Borders::ALL).title("Live Logs"))
            .wrap(Wrap { trim: false });
        frame.render_widget(logs, chunks[2]);

        let footer_text = if let Some(path) = &self.running_log_path {
            format!("Live log file: {path}")
        } else {
            "Please wait. Quitting is disabled while the download job runs.".to_string()
        };

        let footer =
            Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL).title("Info"));
        frame.render_widget(footer, chunks[3]);
    }

    fn render_result(&self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(8),
                Constraint::Min(6),
                Constraint::Length(3),
            ])
            .split(frame.area());

        let (title_text, color) = if let Some(summary) = &self.result {
            if summary.outcome.success {
                ("Download finished successfully", Color::Green)
            } else {
                ("Download failed", Color::Red)
            }
        } else {
            ("Download failed", Color::Red)
        };

        let title = Paragraph::new(Line::from(Span::styled(
            title_text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )))
        .block(Block::default().borders(Borders::ALL).title("Result"));
        frame.render_widget(title, chunks[0]);

        let details_lines = if let Some(summary) = &self.result {
            let mut lines = vec![
                Line::from(format!("Selection reason: {}", summary.reason)),
                Line::from(format!("Chosen URL: {}", summary.chosen_url)),
                Line::from(format!("Output path: {}", summary.output_path)),
                Line::from(format!("Saved log: {}", summary.log_path)),
            ];

            if summary.probes.is_empty() {
                lines.push(Line::from("Probe results: unavailable"));
            } else {
                for probe in &summary.probes {
                    lines.push(Line::from(format!(
                        "Probe {}: {:.2} Mbps over {:.1}s ({} bytes)",
                        protocol_name(probe.protocol),
                        probe.mbps,
                        probe.sample_seconds,
                        probe.sample_bytes
                    )));
                }
            }

            if summary.auth_hint {
                lines.push(Line::from(
                    "Hint: authentication looks invalid. Fill username/password and retry.",
                ));
            }

            lines
        } else {
            vec![Line::from(
                self.result_error
                    .as_deref()
                    .unwrap_or("unknown error while running download"),
            )]
        };

        let details = Paragraph::new(details_lines)
            .block(Block::default().borders(Borders::ALL).title("Details"))
            .wrap(Wrap { trim: true });
        frame.render_widget(details, chunks[1]);

        let log_lines = if let Some(summary) = &self.result {
            let tail = tail_lines(&summary.outcome.combined_log, 14);
            if tail.is_empty() {
                vec![Line::from("(no aria2 output captured)")]
            } else {
                tail.into_iter().map(Line::from).collect()
            }
        } else {
            vec![Line::from("(no output)")]
        };

        let logs = Paragraph::new(log_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("aria2 Log (tail)"),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(logs, chunks[2]);

        let footer = Paragraph::new("r=retry, e=edit inputs, p=profiles, q=quit")
            .block(Block::default().borders(Borders::ALL).title("Help"));
        frame.render_widget(footer, chunks[3]);
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = stdout.execute(LeaveAlternateScreen);
    }
}

pub fn run_tui() -> Result<i32> {
    let config = AppConfig::load().context("failed to load app config")?;
    let mut app = TuiApp::new(config);

    enable_raw_mode().context("failed to enable raw terminal mode")?;
    let mut stdout = io::stdout();
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to initialize terminal")?;

    while !app.should_quit {
        app.poll_worker();
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(Duration::from_millis(120)).context("event poll failed")? {
            if let Event::Key(key) = event::read().context("event read failed")? {
                app.handle_key(key);
            }
        }

        app.on_tick();
    }

    Ok(0)
}

fn run_browser_list_job(request: BrowserListRequest) -> Result<BrowserListSummary> {
    let listing = list_ftp_directory(
        &request.ftp_base_url,
        &request.remote_dir,
        request.credentials.as_ref(),
    )?;

    Ok(BrowserListSummary { listing })
}

fn run_download_job(
    input: DownloadInput,
    events: &mpsc::Sender<DownloadWorkerEvent>,
) -> Result<DownloadSummary> {
    let aria2_path = find_aria2().ok_or(AppError::MissingAria2)?;
    let store = MacKeychainStore;
    let log_path = create_tui_log_path();
    let _ = events.send(DownloadWorkerEvent::LogPath(log_path.display().to_string()));

    let _ = events.send(DownloadWorkerEvent::Status(
        "Preparing URL candidates...".to_string(),
    ));

    let http_url = build_remote_url(&input.http_base_url, &input.remote_path, Protocol::Http)?;
    let ftp_url = build_remote_url(&input.ftp_base_url, &input.remote_path, Protocol::Ftp)?;

    let candidates = resolve_candidates(
        http_url.as_str(),
        ProtocolArg::Auto,
        None,
        Some(ftp_url.as_str()),
    )?;

    let selection = select_candidate_with_probe(&aria2_path, &candidates, ProtocolArg::Auto)?;

    let _ = events.send(DownloadWorkerEvent::Status(format!(
        "Protocol selected: {}. Starting aria2...",
        protocol_name(selection.chosen.protocol)
    )));

    let host = selection
        .chosen
        .url
        .host_str()
        .ok_or_else(|| AppError::MissingHost(selection.chosen.url.to_string()))?
        .to_string();

    let saved_credentials = store.get(&host)?;
    let (credentials, _) = select_credentials(input.credentials.clone(), saved_credentials);

    let plan = build_transfer_plan(
        selection.chosen.url.clone(),
        input.output_dir.clone(),
        input.filename.clone(),
        None,
        selection.best_probe.clone(),
        credentials,
    )?;

    let aria2_args = plan.aria2_args();
    let mut log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("failed to create log file {}", log_path.display()))?;
    let _ = writeln!(
        log_file,
        "fast-movie-dl tui log\nchosen_url={}\noutput_path={}\n",
        plan.chosen_url,
        plan.output_path().display()
    );

    let outcome = execute_aria2_capture_with_sink(&aria2_path, &aria2_args, |is_stderr, line| {
        let rendered = if is_stderr {
            format!("[stderr] {line}")
        } else {
            line.to_string()
        };

        let _ = writeln!(log_file, "{rendered}");
        let _ = events.send(DownloadWorkerEvent::LogLine(rendered));
    })?;

    if outcome.success && input.remember_keychain {
        if let Some(creds) = &plan.credentials {
            store
                .set(&host, creds)
                .with_context(|| format!("failed to save credentials for host {host}"))?;
        }
    }

    Ok(DownloadSummary {
        reason: selection.reason,
        chosen_url: redact_url_for_display(&selection.chosen),
        output_path: plan.output_path().display().to_string(),
        probes: selection.all_probes,
        auth_hint: !outcome.success && looks_like_auth_error(&outcome.combined_log),
        outcome,
        log_path: log_path.display().to_string(),
    })
}

fn parse_base_url(value: &str, expected_protocol: Protocol) -> Result<Url> {
    let parsed = Url::parse(value.trim()).with_context(|| {
        format!(
            "invalid {} base URL: {}",
            protocol_name(expected_protocol),
            value.trim()
        )
    })?;

    if Protocol::from_scheme(parsed.scheme()) != expected_protocol {
        match expected_protocol {
            Protocol::Http => bail!("HTTP base URL must use http or https"),
            Protocol::Ftp => bail!("FTP base URL must use ftp"),
            Protocol::Unknown => bail!("unsupported protocol"),
        }
    }

    Ok(parsed)
}

fn form_credentials(form: &FormState) -> Option<Credentials> {
    if form.username.trim().is_empty() {
        None
    } else {
        Some(Credentials {
            username: form.username.trim().to_string(),
            password: form.password.clone(),
        })
    }
}

fn display_remote_dir(path: &str) -> String {
    let normalized = normalize_remote_path(path);
    if normalized.is_empty() {
        "/".to_string()
    } else {
        format!("/{normalized}")
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

fn build_remote_url(base: &str, remote_path: &str, expected_protocol: Protocol) -> Result<Url> {
    let mut base_url = parse_base_url(base, expected_protocol)?;
    let trimmed_remote_path = remote_path.trim().trim_start_matches('/');

    if trimmed_remote_path.is_empty() {
        bail!("remote path cannot be empty");
    }

    let path = combine_base_and_relative_path(base_url.path(), trimmed_remote_path);
    base_url.set_path(&path);
    Ok(base_url)
}

fn browser_parent_remote_path(current_dir: &str, ftp_base_url: &str) -> String {
    let normalized = normalize_remote_path(current_dir);
    if !normalized.is_empty() {
        return parent_remote_path(&normalized);
    }

    let Ok(base_url) = Url::parse(ftp_base_url.trim()) else {
        return normalized;
    };

    if base_url
        .path()
        .split('/')
        .any(|segment| !segment.is_empty())
    {
        "..".to_string()
    } else {
        normalized
    }
}

fn select_credentials(
    inline: Option<Credentials>,
    saved: Option<Credentials>,
) -> (Option<Credentials>, bool) {
    match (inline, saved) {
        (Some(inline_creds), Some(saved_creds)) => {
            if inline_creds.password.is_empty()
                && (inline_creds.username.is_empty()
                    || inline_creds.username == saved_creds.username)
            {
                (Some(saved_creds), true)
            } else {
                (Some(inline_creds), false)
            }
        }
        (Some(inline_creds), None) => (Some(inline_creds), false),
        (None, Some(saved_creds)) => (Some(saved_creds), true),
        (None, None) => (None, false),
    }
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Http => "HTTP",
        Protocol::Ftp => "FTP",
        Protocol::Unknown => "UNKNOWN",
    }
}

fn redact_url_for_display(candidate: &UrlCandidate) -> String {
    let mut redacted = candidate.url.clone();
    let _ = redacted.set_password(None);
    redacted.to_string()
}

fn create_tui_log_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    std::env::temp_dir().join(format!("fast-movie-dl-tui-{stamp}.log"))
}

fn tail_vec(input: &[String], max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }

    let keep_from = input.len().saturating_sub(max_lines);
    input[keep_from..].to_vec()
}

fn tail_lines(input: &str, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }

    let lines: Vec<&str> = input.lines().collect();
    let keep_from = lines.len().saturating_sub(max_lines);
    lines[keep_from..]
        .iter()
        .map(|line| line.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_remote_url_for_http() {
        let url = build_remote_url(
            "https://files.example.com/base",
            "movies/2026/sample.mkv",
            Protocol::Http,
        )
        .expect("expected valid URL");

        assert_eq!(
            url.as_str(),
            "https://files.example.com/base/movies/2026/sample.mkv"
        );
    }

    #[test]
    fn builds_remote_url_one_parent_above_base() {
        let url = build_remote_url(
            "https://files.example.com/downloads",
            "../incoming/sample.mkv",
            Protocol::Http,
        )
        .expect("expected valid URL");

        assert_eq!(
            url.as_str(),
            "https://files.example.com/incoming/sample.mkv"
        );
    }

    #[test]
    fn build_remote_url_does_not_escape_more_than_one_parent() {
        let url = build_remote_url(
            "https://files.example.com/base/downloads",
            "../../incoming/sample.mkv",
            Protocol::Http,
        )
        .expect("expected valid URL");

        assert_eq!(
            url.as_str(),
            "https://files.example.com/base/incoming/sample.mkv"
        );
    }

    #[test]
    fn browser_parent_allows_one_parent_above_non_root_base() {
        assert_eq!(
            browser_parent_remote_path("", "ftp://files.example.com/downloads"),
            ".."
        );
    }

    #[test]
    fn browser_parent_stays_at_root_for_root_base() {
        assert_eq!(
            browser_parent_remote_path("", "ftp://files.example.com"),
            ""
        );
    }

    #[test]
    fn rejects_invalid_ftp_scheme() {
        let err = build_remote_url(
            "https://files.example.com/base",
            "movies/2026/sample.mkv",
            Protocol::Ftp,
        )
        .expect_err("expected ftp scheme validation error");

        assert!(err.to_string().contains("FTP base URL must use ftp"));
    }

    #[test]
    fn keeps_tail_lines_only() {
        let result = tail_lines("a\nb\nc\nd", 2);
        assert_eq!(result, vec!["c".to_string(), "d".to_string()]);
    }
}
