//! sound-sift — a small TUI to sift through a sound pack.
//!
//! Browse the current directory and its subdirectories, preview audio files
//! (wav/mp3/ogg/flac/opus/...), and delete anything you don't need.
//!
//! Keys:
//!   j/k or ↑/↓     move selection
//!   h/←/Backspace  up a directory
//!   Esc            quit (also cancels filter / closes dialogs)
//!   l/→/Enter      enter dir · play file
//!   space          play / pause
//!   x              stop playback
//!   n / p          play next / previous sound
//!   d / Delete     delete file or folder (asks for confirmation)
//!   /              filter by name
//!   g / G          jump to top / bottom
//!   r              refresh
//!   #              toggle hidden files
//!   s              sort by name / size
//!   ?              toggle this help
//!   q              quit

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use rodio::mixer::Mixer;
use rodio::Player;
use rodio::{DeviceSinkBuilder, play};

/// File extensions treated as playable audio.
const AUDIO_EXTS: &[&str] = &[
    "aac", "aif", "aiff", "flac", "m4a", "mp3", "oga", "ogg", "opus", "spx", "wav", "wv",
];

/// A row in the file list: a subdirectory or an audio file.
#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    kind: EntryKind,
    size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir,
    Audio,
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Browse,
    Filter,
}

/// A delete request waiting for confirmation in the dialog.
struct PendingDelete {
    entry: Entry,
    /// Recursively counted items, present only for directories.
    item_count: Option<usize>,
}

/// The currently playing track and its player.
struct Playing {
    player: Player,
    name: String,
    path: PathBuf,
}

/// All application state for the TUI.
struct App {
    dir: PathBuf,
    all: Vec<Entry>,
    visible: Vec<Entry>,
    selection: usize,
    scroll: usize,
    filter: String,
    mode: Mode,
    show_hidden: bool,
    sort_by_size: bool,
    playing: Option<Playing>,
    pending_delete: Option<PendingDelete>,
    show_help: bool,
    status: String,
    /// Stack of (directory, selection) for every directory we descended into,
    /// so going back up restores the selection we had before.
    nav_history: Vec<(PathBuf, usize)>,
}

impl App {
    /// Create an app rooted at `dir` and load its first listing.
    fn new(dir: PathBuf) -> Result<Self, String> {
        let mut app = App {
            dir,
            all: Vec::new(),
            visible: Vec::new(),
            selection: 0,
            scroll: 0,
            filter: String::new(),
            mode: Mode::Browse,
            show_hidden: false,
            sort_by_size: false,
            playing: None,
            pending_delete: None,
            show_help: false,
            status: String::from("Ready"),
            nav_history: Vec::new(),
        };
        app.refresh()?;
        Ok(app)
    }

    fn current(&self) -> Option<&Entry> {
        self.visible.get(self.selection)
    }

    fn selected_name(&self) -> Option<String> {
        self.current().map(|e| e.name.clone())
    }

    /// Re-read the current directory and rebuild the visible list,
    /// keeping the currently selected entry visible when possible.
    fn refresh(&mut self) -> Result<(), String> {
        self.dir = self.dir
            .canonicalize()
            .map_err(|e| format!("can't access {}: {e}", self.dir.display()))?;
        let all = collect_entries(&self.dir, self.show_hidden)?;
        let kept = self.selected_name();
        self.all = all;
        self.apply_view(kept.as_deref());
        Ok(())
    }

    /// Rebuild `visible` from `all` with the current filter and sort order.
    fn apply_view(&mut self, keep_name: Option<&str>) {
        let f = self.filter.trim().to_lowercase();
        let mut v: Vec<Entry> = if f.is_empty() {
            self.all.clone()
        } else {
            self.all
                .iter()
                .filter(|e| e.name.to_lowercase().contains(&f))
                .cloned()
                .collect()
        };
        sort_entries(&mut v, self.sort_by_size);
        let len = v.len();
        if len > 0 {
            let target = keep_name
                .and_then(|n| v.iter().position(|e| e.name == n))
                .unwrap_or_else(|| self.selection.min(len - 1));
            self.selection = target;
        } else {
            self.selection = 0;
        }
        // Re-anchored on draw by `adjust_scroll`; resetting here prevents a stale
        // scroll offset from one directory being rendered in another.
        self.scroll = 0;
        self.visible = v;
    }

    fn move_sel(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.selection as isize;
        let next = (cur + delta).clamp(0, (self.visible.len() - 1) as isize);
        self.selection = next as usize;
    }

    fn adjust_scroll(&mut self, height: usize) {
        if self.visible.is_empty() {
            self.scroll = 0;
            return;
        }
        let h = height.max(1);
        if self.selection < self.scroll {
            self.scroll = self.selection;
        } else if self.scroll + h <= self.selection {
            self.scroll = self.selection - h + 1;
        }
    }

    fn enter_dir(&mut self) {
        let Some(e) = self.current().cloned() else {
            return;
        };
        if e.kind != EntryKind::Dir {
            return;
        }
        let from = (self.dir.clone(), self.selection);
        self.dir = e.path;
        if let Err(err) = self.refresh() {
            self.status = err;
            return;
        }
        self.selection = 0;
        self.nav_history.push(from);
    }

    fn up_dir(&mut self) {
        let Some(parent) = self.dir.parent() else {
            return;
        };
        // Restore the selection we had in `parent` when we last descended from it.
        let restore = self
            .nav_history
            .last()
            .filter(|(dir, _)| *dir == parent)
            .map(|(_, sel)| *sel);
        if restore.is_some() {
            self.nav_history.pop();
        }
        self.dir = parent.to_path_buf();
        if let Err(err) = self.refresh() {
            self.status = err;
            return;
        }
        if let Some(sel) = restore {
            self.selection = sel.min(self.visible.len().saturating_sub(1));
        }
    }

    /// Stop the current track immediately (no status message).
    fn stop_current(&mut self) {
        if let Some(playing) = self.playing.take() {
            playing.player.stop();
            // Dropping the Player marks it stopped and detaches it from the mixer.
        }
    }

    /// Play an audio entry, replacing whatever is playing.
    fn play_entry(&mut self, e: &Entry, mixer: &Mixer) {
        self.stop_current();
        let name = e.name.clone();
        let file = match fs::File::open(&e.path) {
            Ok(f) => f,
            Err(err) => {
                self.status = format!("✗ can't open {name}: {err}");
                return;
            }
        };
        match play(mixer, file) {
            Ok(player) => {
                self.playing = Some(Playing { player, name: e.name.clone(), path: e.path.clone() });
                self.status = format!("▶ {}", e.name);
            }
            Err(err) => self.status = format!("✗ can't decode {name}: {err}"),
        }
    }

    fn play_selected(&mut self, mixer: &Mixer) {
        let Some(e) = self.current().cloned() else {
            return;
        };
        if e.kind == EntryKind::Audio {
            self.play_entry(&e, mixer);
        }
    }

    /// Play the next (or previous) audio entry relative to the selection, wrapping around.
    fn play_offset(&mut self, delta: isize, mixer: &Mixer) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        let mut i = self.selection as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n as isize);
            if self.visible[i as usize].kind == EntryKind::Audio {
                self.selection = i as usize;
                if let Some(e) = self.current().cloned() {
                    self.play_entry(&e, mixer);
                }
                return;
            }
        }
    }

    fn toggle_pause(&mut self) {
        if let Some(p) = self.playing.as_mut() {
            if p.player.is_paused() {
                p.player.play();
                self.status = format!("▶ {}", p.name);
            } else {
                p.player.pause();
                self.status = format!("⏸ Paused: {}", p.name);
            }
        }
    }

    /// Check for finished tracks once per event loop iteration.
    fn tick(&mut self) {
        let finished = self.playing.as_ref().is_some_and(|p| p.player.empty());
        if !finished {
            return;
        }
        let Some(playing) = self.playing.take() else {
            return;
        };
        self.status = format!("■ Finished: {}", playing.name);
    }

    // ------------------------------------------------------------------
    // Delete
    // ------------------------------------------------------------------

    /// Open the delete confirmation dialog for the selected entry.
    fn request_delete(&mut self) {
        let Some(entry) = self.current().cloned() else {
            return;
        };
        let item_count = if entry.kind == EntryKind::Dir {
            Some(count_contents(&entry.path))
        } else {
            None
        };
        self.pending_delete = Some(PendingDelete { entry, item_count });
    }

    /// Perform the pending delete (only called after the user confirmed).
    fn execute_delete(&mut self) {
        let Some(pd) = self.pending_delete.take() else {
            return;
        };
        let entry = pd.entry;

        // Never leave a deleted file half-playing.
        if self.playing.as_ref().is_some_and(|p| p.path == entry.path) {
            self.stop_current();
        }

        let result = match entry.kind {
            EntryKind::Dir => fs::remove_dir_all(&entry.path),
            EntryKind::Audio => fs::remove_file(&entry.path),
        };
        match result {
            Ok(()) => {
                self.status = format!("Deleted: {}", entry.name);
                let idx = self.selection;
                if let Ok(all) = collect_entries(&self.dir, self.show_hidden) {
                    self.all = all;
                }
                self.apply_view(None);
                self.selection = idx.min(self.visible.len().saturating_sub(1));
            }
            Err(err) => self.status = format!("✗ delete failed: {err}"),
        }
    }

    // ------------------------------------------------------------------
    // Keys
    // ------------------------------------------------------------------

    /// Handle one key press. Returns `true` when the app should quit.
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers, mixer: &Mixer) -> bool {
        if mods.contains(KeyModifiers::CONTROL) {
            return matches!(code, KeyCode::Char('c') | KeyCode::Char('q'));
        }
        // The delete dialog swallows every key until answered.
        if self.pending_delete.is_some() {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.execute_delete(),
                _ => self.pending_delete = None,
            }
            return false;
        }
        if self.show_help {
            if matches!(code, KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')) {
                self.show_help = false;
            }
            return false;
        }
        match self.mode {
            Mode::Filter => {
                self.handle_filter_key(code);
                false
            }
            Mode::Browse => self.handle_browse_key(code, mixer),
        }
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Browse;
                self.apply_view(None);
                self.status = String::from("filter cleared");
            }
            KeyCode::Enter => self.mode = Mode::Browse,
            KeyCode::Backspace => {
                let kept = self.selected_name();
                self.filter.pop();
                self.apply_view(kept.as_deref());
            }
            KeyCode::Char(c) => {
                let kept = self.selected_name();
                self.filter.push(c);
                self.apply_view(kept.as_deref());
            }
            _ => {}
        }
    }

    fn handle_browse_key(&mut self, code: KeyCode, mixer: &Mixer) -> bool {
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                self.up_dir();
            }
            KeyCode::Esc => return true,
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => match self
                .current()
                .map(|e| e.kind)
            {
                Some(EntryKind::Dir) => self.enter_dir(),
                Some(EntryKind::Audio) => self.play_selected(mixer),
                None => {}
            },
            KeyCode::Char(' ') => {
                if self.playing.is_some() {
                    self.toggle_pause();
                } else {
                    self.play_selected(mixer);
                }
            }
            KeyCode::Char('x') => {
                if let Some(name) = self.playing.as_ref().map(|p| p.name.clone()) {
                    self.stop_current();
                    self.status = format!("⏹ Stopped: {name}");
                }
            }
            KeyCode::Char('n') => self.play_offset(1, mixer),
            KeyCode::Char('p') => self.play_offset(-1, mixer),
            KeyCode::Char('d') | KeyCode::Delete => self.request_delete(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('g') => self.selection = 0,
            KeyCode::Char('G') => self.selection = self.visible.len().saturating_sub(1),
            KeyCode::Char('r') => {
                if let Err(err) = self.refresh() {
                    self.status = err;
                }
            }
            KeyCode::Char('#') => {
                self.show_hidden = !self.show_hidden;
                if let Err(err) = self.refresh() {
                    self.status = err;
                }
            }
            KeyCode::Char('s') => {
                self.sort_by_size = !self.sort_by_size;
                let kept = self.selected_name();
                self.apply_view(kept.as_deref());
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        false
    }
}

// ----------------------------------------------------------------------
// Filesystem helpers
// ----------------------------------------------------------------------

/// List subdirectories and audio files in `dir`.
fn collect_entries(dir: &Path, show_hidden: bool) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let entries =
        fs::read_dir(dir).map_err(|e| format!("can't read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        if file_type.is_dir() {
            out.push(Entry { name, path, kind: EntryKind::Dir, size: 0 });
        } else if file_type.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            if AUDIO_EXTS.contains(&ext.as_str()) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(Entry { name, path, kind: EntryKind::Audio, size });
            }
        }
    }
    Ok(out)
}

/// Directories first, then audio files; by name or by size (descending).
fn sort_entries(v: &mut [Entry], by_size: bool) {
    v.sort_by(|a, b| match (a.kind, b.kind) {
        (EntryKind::Dir, EntryKind::Dir) => cmp_name_ci(&a.name, &b.name),
        (EntryKind::Audio, EntryKind::Audio) => {
            if by_size {
                b.size.cmp(&a.size).then_with(|| cmp_name_ci(&a.name, &b.name))
            } else {
                cmp_name_ci(&a.name, &b.name)
            }
        }
        (EntryKind::Dir, EntryKind::Audio) => Ordering::Less,
        (EntryKind::Audio, EntryKind::Dir) => Ordering::Greater,
    });
}

/// Case-insensitive name comparison (unicode-aware via `to_lowercase`).
fn cmp_name_ci(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Recursively count the items contained in a directory (for the delete dialog).
fn count_contents(dir: &Path) -> usize {
    let mut n = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            n += 1;
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                n += count_contents(&entry.path());
            }
        }
    }
    n
}

/// Format a byte count for display (e.g. "2.0 KB").
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < 2 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Format a duration as m:ss for the status bar.
fn fmt_time(t: Duration) -> String {
    let s = t.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

// ----------------------------------------------------------------------
// Rendering
// ----------------------------------------------------------------------

fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(if app.mode == Mode::Filter { 1 } else { 0 }),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    render_top_bar(f, app, chunks[0]);
    if app.mode == Mode::Filter {
        render_filter_bar(f, app, chunks[1]);
    }
    render_list(f, app, chunks[2]);
    render_status_bar(f, app, chunks[3]);
    if app.show_help {
        render_help(f, area);
    }
    if app.pending_delete.is_some() {
        render_delete_dialog(f, app, area);
    }
}

fn render_top_bar(f: &mut Frame, app: &App, area: Rect) {
    let flags = format!(
        "{}{}",
        if app.show_hidden { " hidden" } else { "" },
        if app.sort_by_size { " size-sort" } else { "" },
    );
    let line = Line::from(vec![
        Span::styled(
            " 🔊 sound-sift ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{}  ", app.dir.display()), Style::default().fg(Color::White)),
        Span::styled(
            format!("{} entries", app.visible.len()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(flags, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_filter_bar(f: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled(" filter: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if app.filter.is_empty() { " ".to_string() } else { app.filter.clone() },
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "   Esc cancel · Enter accept",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.visible.iter().map(|e| {
        let is_dir = e.kind == EntryKind::Dir;
        let (icon, style) = if is_dir {
            ("📁", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        } else {
            ("♪", Style::default().fg(Color::White))
        };
        let mut spans = vec![
            Span::styled(format!(" {icon}  "), style),
            Span::styled(e.name.clone(), style),
        ];
        if is_dir {
            spans.push(Span::styled("/", style));
        } else {
            spans.push(Span::styled(
                format!("   {}", human_size(e.size)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut item = ListItem::new(Line::from(spans));
        if app.playing.as_ref().is_some_and(|p| p.path == e.path) {
            item = item.style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        }
        item
    }).collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state =
        ListState::default().with_offset(app.scroll).with_selected(Some(app.selection));
    f.render_stateful_widget(list, area, &mut state);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let (left, style) = match &app.playing {
        Some(p) => {
            let icon = if p.player.is_paused() { "⏸" } else { "▶" };
            (
                format!("{icon} {}  [{}]", p.name, fmt_time(p.player.get_pos())),
                Style::default().fg(Color::Yellow),
            )
        }
        None => (
            app.status.clone(),
            Style::default().fg(if app.status.starts_with('✗') {
                Color::Red
            } else {
                Color::White
            }),
        ),
    };
    f.render_widget(Paragraph::new(left).style(style).left_aligned(), chunks[0]);
    let hints =
        " j/k move · ↵ open/play · ␣ pause · x stop · n/p next · d del · / filter · ? help · Esc/q quit";
    f.render_widget(
        Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)).right_aligned(),
        chunks[1],
    );
}

fn render_help(f: &mut Frame, area: Rect) {
    let rect = Rect::centered(area, Constraint::Length(44), Constraint::Length(17));
    f.render_widget(Clear, rect);
    let lines = [
        "j/k or ↑/↓      move selection",
        "h/←/Backspace   up a directory",
        "l/→/Enter       enter dir · play file",
        "space           play / pause",
        "x               stop playback",
        "n / p           next / previous sound (plays)",
        "d / Delete      delete file or folder (asks)",
        "/               filter by name",
        "g / G           top / bottom",
        "r               refresh",
        "#               show/hide dotfiles",
        "s               sort by name / size",
        "?               toggle this help",
        "q / Esc         quit",
    ];
    let text: Vec<Line> = lines.iter().map(|s| Line::from(s.to_string())).collect();
    let block = Block::bordered()
        .title(" keys ")
        .border_style(Style::default().fg(Color::DarkGray))
        .title_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    f.render_widget(
        Paragraph::new(text).block(block).style(Style::default().fg(Color::White)),
        rect,
    );
}

/// Modal delete-confirmation dialog shown on top of everything else.
fn render_delete_dialog(f: &mut Frame, app: &App, area: Rect) {
    let Some(pd) = &app.pending_delete else {
        return;
    };
    let rect = Rect::centered(area, Constraint::Length(48), Constraint::Length(8));
    f.render_widget(Clear, rect);
    let target = match pd.item_count {
        Some(count) => format!("'{}/' and its {count} items", pd.entry.name),
        None => format!("'{}'", pd.entry.name),
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled(
            format!("Delete {target} ?"),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )]),
        Line::from(Span::styled("This cannot be undone.", Style::default().fg(Color::DarkGray))),
        Line::from(""),
        Line::from(vec![
            Span::styled(" [y] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled("delete    ", Style::default().fg(Color::White)),
            Span::styled("[n] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled("cancel", Style::default().fg(Color::White)),
        ]),
    ];
    let block = Block::bordered()
        .title(" Delete ")
        .border_style(Style::default().fg(Color::Red))
        .title_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
    f.render_widget(
        Paragraph::new(lines).block(block).style(Style::default().fg(Color::White)),
        rect,
    );
}

// ----------------------------------------------------------------------
// Program entry points
// ----------------------------------------------------------------------

const USAGE: &str = "\
sound-sift — browse a sound pack in the terminal and preview sounds

usage: sound-sift [DIR]
       sound-sift --play FILE

options:
  DIR           directory to open (default: current directory)
  --play FILE   play FILE without the TUI, then exit
  -h, --help    show this help

keys:
  j/k move   ↵ open dir / play   space pause   x stop
  n/p next/prev sound   d delete (confirms)   / filter
  # hidden   s sort name/size   ? help   Esc/q quit";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = PathBuf::from(".");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--play" => {
                let Some(path) = args.get(i + 1).cloned().map(PathBuf::from) else {
                    eprintln!("error: --play requires a FILE argument");
                    std::process::exit(2);
                };
                if let Err(err) = play_file(&path) {
                    eprintln!("error: {err}");
                    std::process::exit(1);
                }
                return;
            }
            other if !other.starts_with('-') => dir = PathBuf::from(other),
            other => {
                eprintln!("error: unknown option {other}");
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if let Err(err) = run_tui(dir) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

/// Play a single file without the TUI (used for scripting and quick checks).
fn play_file(path: &Path) -> Result<(), String> {
    let mut device_sink = DeviceSinkBuilder::open_default_sink()
        .map_err(|e| format!("no audio output device: {e}"))?;
    device_sink.log_on_drop(false);
    let mixer = device_sink.mixer();
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let player = play(mixer, file)
        .map_err(|e| format!("can't decode {}: {e}", path.display()))?;
    println!("Playing {} (Ctrl+C to stop)", path.display());
    io::stdout().flush().ok();
    player.sleep_until_end();
    println!("Done.");
    Ok(())
}

/// Open the audio device and run the TUI until the user quits.
fn run_tui(start: PathBuf) -> Result<(), String> {
    let mut device_sink = DeviceSinkBuilder::open_default_sink()
        .map_err(|e| format!("no audio output device: {e}"))?;
    device_sink.log_on_drop(false);
    let mixer = device_sink.mixer();
    let mut app = App::new(start)?;

    install_terminal_guard();
    enable_raw_mode().map_err(|e| e.to_string())?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide).map_err(|e| e.to_string())?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|e| e.to_string())?;

    let mut quit = false;
    while !quit {
        app.tick();
        if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
            if let Event::Key(KeyEvent { code, modifiers, kind, .. }) =
                event::read().map_err(|e| e.to_string())?
            {
                if kind == KeyEventKind::Press {
                    quit = app.handle_key(code, modifiers, mixer);
                }
            }
        }
        let height = terminal.size().map(|s| s.height).unwrap_or(24);
        app.adjust_scroll(height as usize);
        terminal.draw(|f| render(f, &app)).map_err(|e| e.to_string())?;
    }

    if let Some(playing) = app.playing.take() {
        playing.player.stop();
    }
    restore_terminal();
    Ok(())
}

/// Put the terminal back into a usable state: raw mode off, alternate screen left,
/// cursor visible. Safe to call multiple times.
fn restore_terminal() {
    let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Install a panic hook so a mid-run panic doesn't strand the terminal in raw mode.
fn install_terminal_guard() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, size: u64) -> Entry {
        Entry { name: name.into(), path: PathBuf::from(name), kind, size }
    }

    #[test]
    fn dirs_sort_first_case_insensitive() {
        let mut v = vec![
            entry("b.mp3", EntryKind::Audio, 10),
            entry("B_dir", EntryKind::Dir, 0),
            entry("a_dir", EntryKind::Dir, 0),
            entry("a.mp3", EntryKind::Audio, 5),
        ];
        sort_entries(&mut v, false);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a_dir", "B_dir", "a.mp3", "b.mp3"]);
    }

    #[test]
    fn size_sort_is_descending() {
        let mut v = vec![
            entry("small.wav", EntryKind::Audio, 100),
            entry("big.wav", EntryKind::Audio, 900),
            entry("mid.wav", EntryKind::Audio, 500),
        ];
        sort_entries(&mut v, true);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["big.wav", "mid.wav", "small.wav"]);
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    /// Regression test: selection must survive descending and coming back up,
    /// even when the child directory has a different length.
    #[test]
    fn selection_persists_across_navigation() {
        let root = std::env::temp_dir().join(format!("sift-nav-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::create_dir_all(root.join("beta")).unwrap();
        fs::write(root.join("a.wav"), []).unwrap();
        fs::write(root.join("b.wav"), []).unwrap();
        fs::write(root.join("z.wav"), []).unwrap();
        // 3 files inside: moving down there used to corrupt the parent selection.
        for i in 1..=3 {
            fs::write(root.join(format!("alpha/x{i}.wav")), []).unwrap();
        }

        let mut app = App::new(root.clone()).unwrap();
        // Parent list: alpha/, beta/, a.wav, b.wav, z.wav  → pick alpha (3rd visible slot
        // is a file here, so we use the first directory as the stand-in).
        assert_eq!(app.visible.len(), 5);
        app.selection = 0;
        app.enter_dir();
        assert_eq!(app.current().unwrap().name, "x1.wav");
        app.move_sel(2); // sit on the last file in the child (index 2 of 3)
        app.up_dir();
        assert_eq!(app.current().unwrap().name, "alpha");

        // Deeper round trip with a non-zero parent selection.
        app.selection = 1;
        app.enter_dir(); // into beta (empty)
        app.up_dir();
        assert_eq!(app.current().unwrap().name, "beta");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filter_is_case_insensitive() {
        let all = vec![entry("Glass_Ping.wav", EntryKind::Audio, 1), entry("metal.wav", EntryKind::Audio, 2)];
        let f = "glass".to_string();
        let v: Vec<Entry> = all
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&f))
            .cloned()
            .collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Glass_Ping.wav");
    }
}
