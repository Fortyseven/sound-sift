# sound-sift

A small terminal UI for sifting through a sound pack: browse directories, preview audio files (wav/mp3/ogg/flac/opus/…), and delete anything you don't need.

## Build

Requires Rust (edition 2024) and a system audio backend (e.g. `libasound2-dev` / PulseAudio on Linux).

```sh
cargo build --release
```

The binary lands at `target/release/sound-sift`.

## Usage

```
sound-sift [DIR]        open the TUI rooted at DIR (default: current dir)
sound-sift --play FILE  play FILE without the TUI, then exit
```

### Keys

| Key                | Action                                        |
| ------------------ | --------------------------------------------- |
| `j`/`k` or `↑`/`↓` | move selection                                |
| `h`/`←`            | up a directory                                |
| `l`/`→`/`Enter`    | enter dir · play file                         |
| `space`            | play / pause                                  |
| `x`                | stop playback                                 |
| `n` / `p`          | next / previous sound                         |
| `d`                | delete file or folder (asks for confirmation) |
| `/`                | filter by name                                |
| `g` / `G`          | jump to top / bottom                          |
| `r`                | refresh                                       |
| `#`                | toggle hidden files                           |
| `s`                | sort by name / size                           |
| `?`                | help                                          |
| `q` / `Esc`        | quit                                          |

## Test

```sh
cargo test
```
