# fast-movie-dl

Speed-focused CLI for downloading huge movie files over HTTP/HTTPS/FTP using `aria2c`.

## What it does

- Uses `aria2c` with high-throughput defaults for large files (60-90GB and up).
- Auto-resumes interrupted downloads.
- Supports protocol auto mode with short throughput probing when both HTTP and FTP URLs are provided.
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
