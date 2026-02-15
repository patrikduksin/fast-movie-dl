use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};

#[derive(Debug)]
pub struct RunOutcome {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub combined_log: String,
}

pub fn execute_aria2(aria2_path: &str, args: &[String]) -> Result<RunOutcome> {
    let mut command = Command::new(aria2_path);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().context("failed to start aria2c")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture aria2 stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture aria2 stderr")?;

    let (tx, rx) = mpsc::channel::<(bool, String)>();

    let stdout_handle = spawn_stream_thread(stdout, false, tx.clone());
    let stderr_handle = spawn_stream_thread(stderr, true, tx.clone());

    drop(tx);

    let mut log = String::new();
    const LOG_LIMIT: usize = 128 * 1024;
    for (is_stderr, line) in rx {
        if is_stderr {
            let _ = writeln!(std::io::stderr(), "{}", line);
        } else {
            let _ = writeln!(std::io::stdout(), "{}", line);
        }

        append_bounded(&mut log, &line, LOG_LIMIT);
    }

    let status = child.wait().context("failed waiting for aria2c")?;
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    Ok(RunOutcome {
        success: status.success(),
        exit_code: status.code(),
        combined_log: log,
    })
}

fn append_bounded(buf: &mut String, line: &str, limit: usize) {
    if limit == 0 {
        return;
    }

    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(line);

    if buf.len() > limit {
        let overflow = buf.len() - limit;
        let mut split = overflow.min(buf.len());
        while split < buf.len() && !buf.is_char_boundary(split) {
            split += 1;
        }
        buf.drain(..split);
    }
}

pub fn looks_like_auth_error(text: &str) -> bool {
    let low = text.to_lowercase();
    [
        "401",
        "unauthorized",
        "forbidden",
        "530",
        "login incorrect",
        "authentication failed",
        "authorization failed",
        "wrong password",
    ]
    .iter()
    .any(|token| low.contains(token))
}

fn spawn_stream_thread<R: Read + Send + 'static>(
    reader: R,
    is_stderr: bool,
    tx: mpsc::Sender<(bool, String)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let buf_reader = BufReader::new(reader);
        for line in buf_reader.lines() {
            match line {
                Ok(text) => {
                    if tx.send((is_stderr, text)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}
