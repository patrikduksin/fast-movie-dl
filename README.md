# fast-movie-dl

Speed-focused CLI for downloading huge movie files over HTTP/HTTPS/FTP using `aria2c`.

## What it does

- Uses `aria2c` with high-throughput defaults for large files (60-90GB and up).
- Auto-resumes interrupted downloads.
- Supports protocol auto mode with short throughput probing when both HTTP and FTP URLs are provided.
- Includes an interactive manual speed test for HTTP vs FTP using the same remote file path.
- Prompts for credentials on auth failures and stores them in macOS Keychain by host.

## Requirements

- macOS
- Rust toolchain (`cargo`, `rustc`)
- `aria2c`

Install `aria2c`:

```bash
brew install aria2
```

## Build

```bash
cargo build --release
```

Binary path:

```bash
./target/release/fast-movie-dl
```

## Usage

Doctor check:

```bash
fast-movie-dl doctor
```

Basic download:

```bash
fast-movie-dl download "https://files.example.com/movie.mkv"
```

Compare HTTP vs FTP before full download:

```bash
fast-movie-dl download "https://files.example.com/movie.mkv" \
  --ftp-url "ftp://files.example.com/movie.mkv" \
  --protocol auto
```

Run an interactive manual speed test (base URLs + shared remote path):

```bash
fast-movie-dl speed-test
```

Prompt flow:

```text
HTTP base URL (e.g. https://files.example.com)
FTP base URL (e.g. ftp://files.example.com)
Remote path (e.g. movies/2026/sample.mkv)
```

Notes for `speed-test`:

- Shows a live spinner while probing each protocol.
- Reuses URL credentials or saved macOS Keychain credentials by host when available.
- If probe errors look auth-related, it prompts once for credentials and retries that protocol.

Force FTP:

```bash
fast-movie-dl download "ftp://files.example.com/movie.mkv" --protocol ftp
```

Set output directory and file name:

```bash
fast-movie-dl download "https://files.example.com/movie.mkv" \
  --out "/Users/you/Movies" \
  --filename "Movie.2026.4K.mkv"
```

Disable keychain persistence:

```bash
fast-movie-dl download "https://files.example.com/movie.mkv" --no-keychain
```

Dry-run (print planned `aria2c` command only):

```bash
fast-movie-dl download "https://files.example.com/movie.mkv" --dry-run
```

Clear saved credentials for a host:

```bash
fast-movie-dl auth clear --host files.example.com
```

## Notes

- For protocol comparison, provide both candidate URLs explicitly (`--ftp-url` or `--http-url`) so the tool can measure both safely.
- v1 is download-only and does not auto-start playback while downloading.
