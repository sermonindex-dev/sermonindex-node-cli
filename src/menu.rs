//! `sermonindex-node menu` — the interactive menu.
//!
//! WHY THIS EXISTS
//!
//! The CLI could do nearly everything the desktop app does, but only for
//! someone who already knew the command and its exact arguments. What it could
//! not do at all was the thing most people install a node for on an old
//! laptop: "give me everything by this preacher." The desktop app's Bulk
//! Download page does that in two clicks; the CLI had no idea who preached
//! anything (see catalog.rs).
//!
//! This is that page, and the rest of the app's everyday controls, as a
//! full-screen menu: arrow keys to move, Enter to choose, Esc to go back.
//! Nothing here needs the mouse or a desktop. The menu itself never downloads:
//! it writes your picks and settings and asks the node to sweep, and it starts
//! and stops the node — in the background, or through systemd when the node
//! is installed as a service — while the menu stays on screen with the node's
//! activity in the right-hand panel (read from node.log, see nodelog.rs).
//!
//! Works in any terminal: a Mac, a Linux desktop, an SSH session, and the bare
//! Linux console of a laptop with no desktop installed (TERM=linux), where it
//! drops to plain ASCII box lines and symbols that the console font has.

use std::collections::{HashSet, VecDeque};
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;

use crate::catalog::Catalog;
use crate::picks::Picks;
use crate::{cfg, config, system, update};

// ── look ─────────────────────────────────────────────────────────────────────

/// 256-colour palette indexes rather than RGB: macOS Terminal.app ignores
/// 24-bit colour, and the Linux console maps these to its 16 well enough.
#[derive(Clone, Copy)]
struct Theme {
    olive: Color,
    olive_dk: Color,
    gold: Color,
    cream: Color,
    muted: Color,
    green: Color,
    red: Color,
    plain: bool,
    ascii: bool,
}

impl Theme {
    fn detect() -> Theme {
        let plain = std::env::var_os("NO_COLOR").is_some();
        // The Linux text console's font has no rounded corners and few of the
        // symbols below; draw with what it does have.
        let ascii = std::env::var("TERM").map(|t| t == "linux" || t == "dumb").unwrap_or(false);
        let c = |n: u8| if plain { Color::Reset } else { Color::Indexed(n) };
        Theme {
            olive: c(143),
            olive_dk: c(58),
            gold: c(178),
            cream: c(230),
            muted: c(245),
            green: c(71),
            red: c(167),
            plain,
            ascii,
        }
    }
    fn g(&self, fancy: &'static str, ascii: &'static str) -> &'static str {
        if self.ascii {
            ascii
        } else {
            fancy
        }
    }
    fn border(&self) -> BorderType {
        if self.ascii {
            BorderType::Plain
        } else {
            BorderType::Rounded
        }
    }
    fn hl(&self) -> Style {
        if self.plain {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.olive_dk).fg(self.cream).add_modifier(Modifier::BOLD)
        }
    }
    fn block(&self, title: &str) -> Block<'static> {
        let b = Block::bordered()
            .border_type(self.border())
            .border_style(Style::new().fg(self.olive));
        if title.is_empty() {
            b
        } else {
            b.title(Line::from(Span::styled(
                format!(" {title} "),
                Style::new().fg(self.gold).add_modifier(Modifier::BOLD),
            )))
        }
    }
}

// The wordmark: "sermon" in a cream pill with "index" beside it — the shape
// of the logo on the website and in the app, drawn in quarter-block letters.
// The Linux text console's font has no quarter blocks, so there it falls back
// to the name in a plain reversed pill (see draw()).
const SERMON: &[&str] = &["▞▀▘▞▀▖▙▀▖▛▚▀▖▞▀▖▛▀▖", "▝▀▖▛▀ ▌  ▌▐ ▌▌ ▌▌ ▌", "▀▀ ▝▀▘▘  ▘▝ ▘▝▀ ▘ ▘"];
const INDEX: &[&str] = &["▗      ▌      ", "▄ ▛▀▖▞▀▌▞▀▖▚▗▘", "▐ ▌ ▌▌ ▌▛▀ ▗▚ ", "▀▘▘ ▘▝▀▘▝▀▘▘ ▘"];
const LOGO_W: u16 = 19 + 2 + 1 + 14 + 2;

fn logo_lines(t: &Theme) -> Vec<Line<'static>> {
    let h = SERMON.len().max(INDEX.len());
    let sw = SERMON[0].chars().count();
    let pill = if t.plain {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().bg(t.cream).fg(t.olive_dk)
    };
    (0..h)
        .map(|i| {
            let s = i.checked_sub(h - SERMON.len()).map(|k| SERMON[k]).unwrap_or("");
            let x = i.checked_sub(h - INDEX.len()).map(|k| INDEX[k]).unwrap_or("");
            Line::from(vec![
                Span::raw(" "),
                Span::styled(format!(" {s:<sw$} "), pill),
                Span::raw(" "),
                Span::styled(x.to_string(), Style::new().fg(t.cream)),
            ])
        })
        .collect()
}

fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_bytes(b: u64) -> String {
    let f = b as f64;
    const K: f64 = 1024.0;
    if f >= K * K * K * K {
        format!("{:.1} TB", f / (K * K * K * K))
    } else if f >= K * K * K {
        format!("{:.1} GB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.0} MB", f / (K * K))
    } else {
        format!("{:.0} KB", f / K)
    }
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        format!("{s:<w$}")
    } else {
        let mut o: String = s.chars().take(w.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

fn bar(t: &Theme, pct: f64, w: usize) -> String {
    let fill = ((pct / 100.0) * w as f64).round().clamp(0.0, w as f64) as usize;
    format!("{}{}", t.g("█", "#").repeat(fill), t.g("░", ".").repeat(w - fill))
}

// ── background work ──────────────────────────────────────────────────────────

enum Msg {
    Stats(Option<Value>),
    Held(HashSet<String>),
    Disk((u64, u64)),
    Lan(Option<String>),
    Catalog(std::result::Result<Catalog, String>),
    Latest(Option<String>),
    Said(String),
    /// The console's seed-access answer (None: could not ask).
    Access(Option<crate::heartbeat::SeedStatus>),
    /// A seed-access request went out: Ok(already approved?) or why not.
    SeedSent(std::result::Result<bool, String>),
}

/// The running node's /stats, over plain HTTP to localhost. No curl needed.
fn fetch_stats() -> Option<Value> {
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", config::DASHBOARD_PORT).parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
    s.write_all(b"GET /stats HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let i = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let v: Value = serde_json::from_slice(&buf[i + 4..]).ok()?;
    v.get("node").is_some().then_some(v)
}

/// Ids of every complete file on disk. Finished downloads are renamed into
/// place from `.part`, so a file with its final name is a whole file.
fn scan_held() -> HashSet<String> {
    let dir = config::downloads_dir(&config::load_settings());
    let mut out = HashSet::new();
    let Ok(shards) = std::fs::read_dir(&dir) else { return out };
    for sh in shards.flatten() {
        let Ok(files) = std::fs::read_dir(sh.path()) else { continue };
        for f in files.flatten() {
            let name = f.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_suffix(".mp3").or_else(|| name.strip_suffix(".mp4")) {
                out.insert(id.to_string());
            }
        }
    }
    out
}

fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .ok()
}

fn spawn_workers(tx: Sender<Msg>) {
    let t1 = tx.clone();
    std::thread::spawn(move || {
        let mut n = 0u64;
        loop {
            if t1.send(Msg::Stats(fetch_stats())).is_err() {
                return;
            }
            if n % 8 == 0 && t1.send(Msg::Held(scan_held())).is_err() {
                return;
            }
            if n % 4 == 0 {
                let dir = config::downloads_dir(&config::load_settings());
                if t1.send(Msg::Disk(system::disk_space(&dir))).is_err() {
                    return;
                }
            }
            if n % 30 == 0 && t1.send(Msg::Lan(lan_ip())).is_err() {
                return;
            }
            n += 1;
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    std::thread::spawn(move || {
        // Show whatever is cached at once; refresh from the CDN when it is
        // more than a day old (or missing) — the library grows weekly, not
        // minute to minute.
        let cached = Catalog::load_cached();
        let fresh_enough = Catalog::cache_age().map(|a| a < Duration::from_secs(86_400)).unwrap_or(false);
        let have_cache = cached.is_some();
        let unverified = cached.as_ref().map(|c| c.unverified).unwrap_or(false);
        if let Some(c) = cached {
            let _ = tx.send(Msg::Catalog(Ok(c)));
        }
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let Some(client) = client() else { return };
        if !unverified && !(have_cache && fresh_enough) {
            let r = rt.block_on(Catalog::fetch(&client)).map_err(|e| format!("{e:#}"));
            if r.is_ok() || !have_cache {
                let _ = tx.send(Msg::Catalog(r));
            }
        }
        let _ = tx.send(Msg::Latest(rt.block_on(update::check(&client))));
    });
}

/// This machine's address on the local network, for "open the dashboard on
/// your phone". Connecting a UDP socket sends nothing; it only asks the OS
/// which interface it would use.
fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("1.1.1.1:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v) if !v.is_loopback() && !v.is_unspecified() => Some(v.to_string()),
        _ => None,
    }
}

fn dash_url(host: &str) -> String {
    format!("http://{host}:{}/", config::DASHBOARD_PORT)
}

// ── the node's own output ────────────────────────────────────────────────────

/// The tail of node.log, read a little at a time as it grows.
struct LogTail {
    pos: u64,
    partial: String,
    lines: VecDeque<String>,
}

impl LogTail {
    const KEEP: usize = 400;

    fn new() -> LogTail {
        LogTail { pos: 0, partial: String::new(), lines: VecDeque::new() }
    }

    fn poll(&mut self) {
        self.poll_path(&crate::nodelog::path());
    }

    fn poll_path(&mut self, p: &std::path::Path) {
        let Ok(len) = std::fs::metadata(p).map(|m| m.len()) else { return };
        if len < self.pos {
            // Rotated (or replaced): start again at the top of the new file.
            self.pos = 0;
            self.partial.clear();
        }
        if len == self.pos {
            return;
        }
        // Never read more than the last 128 KB in one go — on first open that
        // is all the panel could ever show anyway.
        let from = self.pos.max(len.saturating_sub(128 * 1024));
        let Ok(mut f) = std::fs::File::open(p) else { return };
        if f.seek(SeekFrom::Start(from)).is_err() {
            return;
        }
        let mut buf = Vec::with_capacity((len - from) as usize);
        if f.take(len - from).read_to_end(&mut buf).is_err() {
            return;
        }
        self.pos = from + buf.len() as u64;
        let text = String::from_utf8_lossy(&buf);
        let mut all = std::mem::take(&mut self.partial);
        all.push_str(&text);
        let mut parts: Vec<&str> = all.split('\n').collect();
        let last = parts.pop().unwrap_or("");
        for l in parts {
            let l = strip_ansi(l.trim_end_matches('\r'));
            if l.trim().is_empty() && self.lines.back().map(|b| b.trim().is_empty()).unwrap_or(true) {
                continue;
            }
            self.lines.push_back(l);
        }
        self.partial = last.to_string();
        while self.lines.len() > Self::KEEP {
            self.lines.pop_front();
        }
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\t' {
            out.push_str("  ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

// ── starting and stopping the node ───────────────────────────────────────────

/// Installed as a systemd service (install.sh does this on Linux)? Then the
/// service is THE node, and the menu starts and stops it rather than running
/// a second copy beside it.
fn service_installed() -> bool {
    cfg!(target_os = "linux")
        && [
            "/etc/systemd/system/sermonindex-node.service",
            "/lib/systemd/system/sermonindex-node.service",
            "/usr/lib/systemd/system/sermonindex-node.service",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).exists())
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn systemctl(verb: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if !is_root() {
        v.push("sudo".into());
    }
    v.extend(["systemctl".to_string(), verb.to_string(), "sermonindex-node".to_string()]);
    v
}

/// Start `sermonindex-node start` detached from this terminal: its own
/// session, no terminal, output only to node.log. It keeps running after the
/// menu closes, and survives the terminal window being shut.
fn spawn_background() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    c.arg("start").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = c.spawn()?;
    // Reap it if it exits while the menu is still open.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Ask a node this user started (not the service) to stop.
fn signal_stop(pid: u64) -> std::result::Result<(), String> {
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EPERM) {
            return Err("That node belongs to another user — stop it from their account.".into());
        }
        Err(format!("Could not stop the node: {e}"))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err("Stop the node from the window it runs in (Ctrl-C).".into())
    }
}

/// Open a web address in this computer's browser, if it has one.
fn open_url(url: &str) -> std::result::Result<(), String> {
    let prog = if cfg!(target_os = "macos") {
        "open"
    } else {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("No desktop in this session (SSH or the text console) — type the address into a \
                        browser on another device instead."
                .into());
        }
        "xdg-open"
    };
    match Command::new(prog).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut ch) => {
            std::thread::spawn(move || {
                let _ = ch.wait();
            });
            Ok(())
        }
        Err(e) => Err(format!("Could not open a browser ({prog}: {e}). Type the address in instead.")),
    }
}

/// Ask the console whether this machine is an approved seed node — now, and
/// every two minutes after, so approval shows up while the menu is open.
fn spawn_access_watch(tx: Sender<Msg>, node_id: String) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let Some(client) = client() else { return };
        loop {
            let a = rt.block_on(crate::heartbeat::seed_status(&client, &node_id));
            let approved = a.as_ref().map(|s| s.enabled).unwrap_or(false);
            if tx.send(Msg::Access(a)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(if approved { 900 } else { 120 }));
        }
    });
}

fn spawn_seed_request(tx: Sender<Msg>, node_id: String, email: String) {
    std::thread::spawn(move || {
        let r = (|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
            let client = client().ok_or("no network client")?;
            match rt.block_on(crate::heartbeat::request_seed_access(&client, &node_id, &email)) {
                Some((enabled, _)) => Ok(enabled),
                None => Err(format!(
                    "Could not reach SermonIndex. Check the internet connection and try again, or email {} \
                     with this machine's id: {node_id}",
                    config::SEED_CONTACT_EMAIL
                )),
            }
        })();
        let _ = tx.send(Msg::SeedSent(r));
    });
}

fn spawn_update_check(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let Some(client) = client() else { return };
        let _ = tx.send(Msg::Latest(rt.block_on(update::check(&client))));
    });
}

// ── state ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Screen {
    Home,
    Status,
    Speakers,
    Speaker(usize),
    Sermons(usize),
    Discover,
    MyPicks,
    Scope,
    Settings,
    Dashboard,
    Seed,
    Connections,
    Updates,
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HomeAct {
    Seed,
    Connections,
    Status,
    Speakers,
    Discover,
    MyPicks,
    Scope,
    Settings,
    Dashboard,
    Updates,
    Start,
    Stop,
    Restart,
    Help,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpkAct {
    Audio,
    All,
    Choose,
    Stop,
    Back,
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Toggle,
    /// A short list of sensible values, plus "Custom…" when `custom` is set.
    Preset(&'static [(&'static str, &'static str)]),
}

/// How a custom entry is turned into what `config` takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    /// As typed.
    Plain,
    /// Typed in Mbps (what internet plans quote), stored as KB/s.
    Mbps,
}

struct Setting {
    label: &'static str,
    key: &'static str,
    kind: Kind,
    /// One line under the list: what this is for.
    hint: &'static str,
    /// The instructions shown when typing a custom value. Empty = no custom.
    custom: &'static str,
    unit: Unit,
}

const CUSTOM: &str = "custom";

const SETTINGS: &[Setting] = &[
    Setting { label: "Upload speed limit", key: "upload",
        kind: Kind::Preset(&[
            ("Unlimited", "off"),
            ("1 Mbps — barely noticeable", "125"),
            ("5 Mbps", "625"),
            ("10 Mbps", "1250"),
            ("25 Mbps", "3125"),
            ("Custom…", CUSTOM),
        ]),
        hint: "How fast this node may share with others. Lower it if the internet feels slow while it runs.",
        custom: "Type the most this node may upload, in Mbps (megabits per second) — the same unit \
                 your internet plan uses for its upload speed.\n\nExample: type 3 to share at up to 3 Mbps.\n\
                 Type 0 for no limit.",
        unit: Unit::Mbps },
    Setting { label: "Seeding hours", key: "schedule",
        kind: Kind::Preset(&[
            ("Around the clock", "off"),
            ("Nights — 10 pm to 7 am", "22:00-07:00"),
            ("Evenings and nights — 6 pm to 8 am", "18:00-08:00"),
            ("Daytime — 8 am to 6 pm", "08:00-18:00"),
            ("Custom…", CUSTOM),
        ]),
        hint: "When this node shares. Outside these hours it rests.",
        custom: "Type the start time and the end time on the 24-hour clock, with a dash between them.\n\n\
                 Example: 23:00-06:30 shares from 11 pm until 6:30 in the morning.\n\
                 Type off to share all day.",
        unit: Unit::Plain },
    Setting { label: "Monthly upload limit", key: "monthly-cap",
        kind: Kind::Preset(&[
            ("No limit", "off"),
            ("100 GB a month", "100"),
            ("250 GB a month", "250"),
            ("500 GB a month", "500"),
            ("1 TB a month", "1024"),
            ("2 TB a month", "2048"),
            ("Custom…", CUSTOM),
        ]),
        hint: "Stop sharing for the rest of the month after this much. Useful if your internet plan has a data allowance.",
        custom: "Type a number of gigabytes (GB) — just the number.\n\n\
                 Example: 750 means: after 750 GB has been shared this month, stop until the 1st. \
                 If your internet plan has a monthly allowance, about half of it is a safe choice.\n\
                 Type 0 for no limit.",
        unit: Unit::Plain },
    Setting { label: "Always keep free on the disk", key: "keep-free",
        kind: Kind::Preset(&[
            ("5 GB — the least it will keep", "5"),
            ("10 GB (recommended)", "10"),
            ("25 GB", "25"),
            ("50 GB", "50"),
            ("100 GB", "100"),
            ("Custom…", CUSTOM),
        ]),
        hint: "Downloads pause before the drive gets this full, so the rest of the computer keeps working.",
        custom: "Type how many gigabytes (GB) must always stay empty on the drive the sermons are on — \
                 just the number, 5 or more.\n\nExample: 20 keeps 20 GB free for the computer itself.",
        unit: Unit::Plain },
    Setting { label: "Library folder", key: "dir", kind: Kind::Text,
        hint: "Where the sermons are stored. Choose a folder on a big drive.",
        custom: "Type the full path of the folder to store sermons in. The folder must already exist.\n\n\
                 Example: /media/greg/BigDrive/sermons\n\
                 Files already downloaded are not moved — restart the node afterwards.",
        unit: Unit::Plain },
    Setting { label: "Downloads at once", key: "downloads",
        kind: Kind::Preset(&[
            ("1 — gentlest on a slow connection", "1"),
            ("2", "2"),
            ("4 (recommended)", "4"),
            ("8 — fast connection", "8"),
            ("Custom…", CUSTOM),
        ]),
        hint: "How many files to fetch at the same time.",
        custom: "Type how many files to download at the same time, a number from 1 to 16.\n\n\
                 Example: 3. Lower is gentler on a slow connection.",
        unit: Unit::Plain },
    Setting { label: "Download source", key: "source",
        kind: Kind::Preset(&[
            ("Automatic (recommended)", "auto"),
            ("Archive.org first", "archive"),
            ("SermonIndex CDN first", "cdn"),
        ]),
        hint: "Where new files come from first. Every source is tried before a file is given up on.",
        custom: "", unit: Unit::Plain },
    Setting { label: "Find peers through DHT", key: "dht", kind: Kind::Toggle,
        hint: "Finds more people to share with. Turn off only to save memory on a very small machine.",
        custom: "", unit: Unit::Plain },
    Setting { label: "Open my router port automatically", key: "natpmp", kind: Kind::Toggle,
        hint: "Asks the router (NAT-PMP/PCP) to let other nodes reach this one.",
        custom: "", unit: Unit::Plain },
    Setting { label: "Share with others (P2P)", key: "p2p", kind: Kind::Toggle,
        hint: "Seed what this node holds. Off makes it download-only.",
        custom: "", unit: Unit::Plain },
    Setting { label: "Files shared at the same moment", key: "window",
        kind: Kind::Preset(&[
            ("Automatic for this machine (recommended)", "auto"),
            ("1,000 — 1 to 2 GB of memory", "1000"),
            ("2,000 — 4 GB of memory", "2000"),
            ("4,000 — 8 GB of memory", "4000"),
            ("8,000 — 16 GB or more", "8000"),
            ("Custom…", CUSTOM),
        ]),
        hint: "Lower uses less memory; the rest of the library takes turns.",
        custom: "Type how many files to share at one moment, e.g. 3000 (at least 100).\n\n\
                 Each one uses a little memory, so smaller numbers suit smaller machines. \
                 The rest of the library takes turns.",
        unit: Unit::Plain },
];

struct Edit {
    idx: usize,
    value: String,
    error: Option<String>,
}

/// One choice in a pick-one box.
#[derive(Clone, Debug, PartialEq)]
enum Pick {
    Start,
    GoScope,
    GoSpeakers,
    GoPicks,
    GoSeed,
    Set(usize, String),
    Custom(usize),
    Cancel,
}

/// A pick-one box: a few lines of explanation, then the choices.
struct Choose {
    title: String,
    lines: Vec<String>,
    opts: Vec<(String, Pick)>,
    current: Option<usize>,
    state: ListState,
}

enum Action {
    AddSpeakers(Vec<usize>),
    RemoveSpeaker(String),
}

struct Confirm {
    title: String,
    lines: Vec<String>,
    action: Action,
}

enum Overlay {
    None,
    Edit(Edit),
    /// Typing the email for a seed-access request.
    Email(String, Option<String>),
    Confirm(Confirm),
    Choose(Choose),
}

#[derive(PartialEq, Eq)]
enum After {
    Nothing,
    Upgrade,
}

struct App {
    t: Theme,
    tx: Sender<Msg>,
    stack: Vec<Screen>,
    home: ListState,
    stats: Option<Value>,
    held: HashSet<String>,
    catalog: Option<Catalog>,
    catalog_err: Option<String>,
    latest: Option<Option<String>>,
    picks: Picks,
    settings: Value,
    spk: ListState,
    query: String,
    sort_count: bool,
    view: Vec<usize>,
    act: ListState,
    srm: ListState,
    disc: Vec<usize>,
    disc_state: ListState,
    rng: u64,
    pk: ListState,
    scope_state: ListState,
    set_state: ListState,
    upd_state: ListState,
    dash_state: ListState,
    overlay: Overlay,
    toast: Option<(String, Instant)>,
    after: After,
    quit: bool,
    log: LogTail,
    /// (free, total) bytes on the library's drive.
    disk: (u64, u64),
    lan: Option<String>,
    /// A command that needs the real terminal (sudo asks for a password):
    /// the event loop steps out of the menu, runs it, and comes back.
    run_cmd: Option<(String, Vec<String>)>,
    node_id: String,
    /// The console's answer about seed access (None until it replies).
    access: Option<bool>,
    /// "pending" / "denied" / "none" / "approved" / "unknown", once asked.
    seed_word: Option<String>,
    declined_at: Option<String>,
    access_err: bool,
    seed_state: ListState,
    sending: bool,
    conn_state: ListState,
    /// A connection test in flight: when it was asked for, and the time of
    /// the test result we had before (a newer one means it has answered).
    testing: Option<(Instant, u64)>,
}

fn sel(i: usize) -> ListState {
    let mut s = ListState::default();
    s.select(Some(i));
    s
}

impl App {
    fn new(tx: Sender<Msg>) -> App {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42)
            | 1;
        App {
            t: Theme::detect(),
            tx,
            stack: vec![Screen::Home],
            home: sel(0),
            stats: None,
            held: HashSet::new(),
            catalog: None,
            catalog_err: None,
            latest: None,
            picks: Picks::load(),
            settings: config::load_settings(),
            spk: sel(0),
            query: String::new(),
            sort_count: true,
            view: Vec::new(),
            act: sel(0),
            srm: sel(0),
            disc: Vec::new(),
            disc_state: sel(0),
            rng: seed,
            pk: sel(0),
            scope_state: sel(0),
            set_state: sel(0),
            upd_state: sel(0),
            dash_state: sel(0),
            overlay: Overlay::None,
            toast: None,
            after: After::Nothing,
            quit: false,
            log: LogTail::new(),
            disk: system::disk_space(&config::downloads_dir(&config::load_settings())),
            lan: None,
            run_cmd: None,
            node_id: {
                let mut s = config::load_settings();
                config::node_id(&mut s)
            },
            access: None,
            seed_word: None,
            declined_at: None,
            access_err: false,
            seed_state: sel(0),
            sending: false,
            conn_state: sel(0),
            testing: None,
        }
    }

    fn screen(&self) -> Screen {
        *self.stack.last().unwrap_or(&Screen::Home)
    }
    fn push(&mut self, s: Screen) {
        self.stack.push(s);
    }
    fn back(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }
    fn say(&mut self, m: impl Into<String>) {
        self.toast = Some((m.into(), Instant::now()));
    }
    fn running(&self) -> bool {
        self.stats.is_some()
    }
    fn scope(&self) -> String {
        config::seed_scope(&self.settings)
    }
    fn rand(&mut self) -> u64 {
        // xorshift64 — plenty for "show me ten speakers I haven't heard of".
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Stats(s) => {
                self.stats = s;
                if let Some((asked, before)) = self.testing {
                    let at = self.probe_at();
                    if at > before {
                        self.testing = None;
                        let m = self.probe_summary();
                        self.say(m);
                    } else if asked.elapsed() > Duration::from_secs(90) {
                        self.testing = None;
                        self.say("The test didn't report back — is the node still running? Try again in a minute.");
                    }
                }
            }
            Msg::Held(h) => self.held = h,
            Msg::Catalog(Ok(c)) => {
                self.catalog = Some(c);
                self.catalog_err = None;
                self.rebuild_view();
                if self.disc.is_empty() {
                    self.shuffle();
                }
            }
            Msg::Catalog(Err(e)) => {
                if self.catalog.is_none() {
                    self.catalog_err = Some(e);
                }
            }
            Msg::Latest(v) => self.latest = Some(v),
            Msg::Disk(d) => self.disk = d,
            Msg::Lan(l) => self.lan = l,
            Msg::Said(m) => self.say(m),
            Msg::Access(a) => match a {
                Some(st) => {
                    let g = st.enabled;
                    let was = self.granted();
                    let was_word = self.seed_word.clone();
                    self.seed_word = Some(st.status.clone());
                    self.declined_at = st.declined_at.clone();
                    if st.status == "denied" && was_word.as_deref() == Some("pending") {
                        self.say("Your seed node request was declined — see the Seed node page.");
                    }
                    self.access = Some(g);
                    self.access_err = false;
                    // Keep the node's copy in step, so it agrees even offline.
                    let mut s = config::load_settings();
                    if s.get("seed_access_granted").and_then(|v| v.as_bool()) != Some(g) {
                        s["seed_access_granted"] = serde_json::json!(g);
                        let _ = config::save_settings(&s);
                        self.settings = s;
                    }
                    if g && !was {
                        self.say("This machine is approved as a seed node — open Seed node to choose what it holds.");
                    }
                }
                None => self.access_err = true,
            },
            Msg::SeedSent(r) => {
                self.sending = false;
                match r {
                    Ok(true) => {
                        self.access = Some(true);
                        self.say("This machine is already approved — choose what it holds below.");
                    }
                    Ok(false) => {
                        self.seed_word = Some("pending".into());
                        self.declined_at = None;
                        self.say(
                            "Request sent. A person reviews it on the SermonIndex console; this screen updates by itself.",
                        )
                    }
                    Err(e) => self.say(e),
                }
            }
        }
    }

    fn rebuild_view(&mut self) {
        let Some(cat) = &self.catalog else { return };
        let q = self.query.to_lowercase();
        let mut v: Vec<usize> = cat
            .speakers
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.sermons.is_empty() && (q.is_empty() || s.name.to_lowercase().contains(&q)))
            .map(|(i, _)| i)
            .collect();
        if self.sort_count {
            v.sort_by(|&a, &b| cat.speakers[b].sermons.len().cmp(&cat.speakers[a].sermons.len()));
        } else {
            v.sort_by(|&a, &b| cat.speakers[a].name.to_lowercase().cmp(&cat.speakers[b].name.to_lowercase()));
        }
        self.view = v;
        self.spk.select(Some(0));
    }

    fn shuffle(&mut self) {
        let mut pool: Vec<usize> = match &self.catalog {
            Some(c) => (0..c.speakers.len()).filter(|&i| c.speakers[i].sermons.len() >= 3).collect(),
            None => return,
        };
        let mut out = Vec::new();
        while out.len() < 10 && !pool.is_empty() {
            let k = (self.rand() as usize) % pool.len();
            out.push(pool.swap_remove(k));
        }
        self.disc = out;
        self.disc_state.select(Some(0));
    }

    fn held_of(&self, si: usize) -> (usize, usize) {
        let Some(cat) = &self.catalog else { return (0, 0) };
        let sp = &cat.speakers[si];
        let held = sp.sermons.iter().filter(|&&k| self.held.contains(&cat.sermons[k].id)).count();
        (held, sp.sermons.len())
    }

    /// Write picks and ask a running node to sweep now.
    fn save_picks(&mut self, what: &str) {
        if let Err(e) = self.picks.save() {
            self.say(format!("Could not save your picks: {e}"));
            return;
        }
        let _ = std::fs::create_dir_all(config::data_dir());
        let _ = std::fs::write(crate::refresh_flag(), b"");
        let tail = if self.running() {
            "The node will start fetching within a minute."
        } else {
            "Choose Start the node in the menu to download them."
        };
        let mut msg = format!("{what} {tail}");
        if let Some((_, need)) = self.still_needed(&self.effective_scope()) {
            if need > self.room() {
                msg.push_str(&format!(
                    " Note: that is {} more, and only {} fits on this drive.",
                    fmt_bytes(need),
                    fmt_bytes(self.room())
                ));
            }
        }
        self.say(msg);
    }

    /// Approved as a seed node? The console's answer, else the last one saved.
    fn granted(&self) -> bool {
        self.access.unwrap_or_else(|| config::seed_access_cached(&self.settings))
    }

    /// The scope downloads actually follow: the library scopes fall back to
    /// the picks until the machine is approved.
    fn effective_scope(&self) -> String {
        let s = self.scope();
        if (s == "audio" || s == "full") && !self.granted() {
            "picks".into()
        } else {
            s
        }
    }

    /// Size of a whole scope in the catalogue: (files, bytes).
    fn library_size(&self, scope: &str) -> Option<(usize, u64)> {
        let cat = self.catalog.as_ref()?;
        let (mut n, mut b) = (0usize, 0u64);
        for sm in &cat.sermons {
            if scope == "full" || !sm.video {
                n += 1;
                b += sm.size;
            }
        }
        Some((n, b))
    }

    fn lib_label(&self, scope: &str) -> String {
        match self.library_size(scope) {
            Some((_, b)) => fmt_bytes(b),
            None if scope == "full" => "~2.4 TB".into(),
            None => "~373 GB".into(),
        }
    }

    /// Space the node leaves free on the library drive.
    fn keep_free(&self) -> u64 {
        config::keep_free_bytes(&self.settings)
    }

    /// What can still be downloaded before the node stops for space.
    fn room(&self) -> u64 {
        self.disk.0.saturating_sub(self.keep_free())
    }

    /// Files and bytes this node would still have to download under `scope`
    /// (plus the picks). None until the catalogue has loaded.
    fn still_needed(&self, scope: &str) -> Option<(usize, u64)> {
        let cat = self.catalog.as_ref()?;
        let picked = self.picks.wanted_ids(Some(cat));
        let (mut n, mut b) = (0usize, 0u64);
        for sm in &cat.sermons {
            let want = match scope {
                "full" => true,
                "audio" => !sm.video || picked.contains(&sm.id),
                _ => picked.contains(&sm.id),
            };
            if want && !self.held.contains(&sm.id) {
                n += 1;
                b += sm.size;
            }
        }
        Some((n, b))
    }

    /// "230 GB free (10 GB kept for the computer)".
    fn disk_words(&self) -> String {
        let (free, total) = self.disk;
        if total == 0 {
            return "unknown".into();
        }
        format!("{} free of {}  ·  {} always kept free", fmt_bytes(free), fmt_bytes(total), fmt_bytes(self.keep_free()))
    }

    fn node_is_service(&self) -> bool {
        match &self.stats {
            Some(s) => s["node"]["service"].as_bool().unwrap_or(false),
            None => service_installed(),
        }
    }

    /// Open a top-level section, keeping the menu on the left in step.
    fn goto(&mut self, a: HomeAct) {
        if let Some(i) = self.home_items().iter().position(|(x, _, _)| *x == a) {
            self.home.select(Some(i));
        }
        self.stack.truncate(1);
        match a {
            HomeAct::Speakers => self.push(Screen::Speakers),
            HomeAct::MyPicks => self.push(Screen::MyPicks),
            HomeAct::Seed => {
                self.seed_state.select(Some(0));
                self.push(Screen::Seed)
            }
            HomeAct::Scope => {
                let cur = match self.scope().as_str() {
                    "picks" => 0,
                    "full" => 2,
                    _ => 1,
                };
                self.scope_state.select(Some(cur));
                self.push(Screen::Scope)
            }
            _ => {}
        }
    }

    /// Before starting: say plainly what the node is about to download and
    /// whether it fits, with a way out to change it first.
    fn start_prompt(&mut self) {
        let chosen = self.scope();
        if (chosen == "audio" || chosen == "full") && !self.granted() {
            let picks = self.still_needed("picks");
            let mut lines = vec![
                format!(
                    "This computer is set to hold the whole {} library. That makes it a seed node, \
                     which needs approval for this machine first — it doesn't have it yet.",
                    if chosen == "full" { "audio and video" } else { "audio" }
                ),
                String::new(),
            ];
            lines.push(match picks {
                Some((0, _)) | None => "Until it is approved the node only downloads your picks, and you haven't picked anything yet.".into(),
                Some((n, b)) => format!("Until it is approved it downloads only your picks: {} ({}).", plural(n, "file", "files"), fmt_bytes(b)),
            });
            self.overlay = Overlay::Choose(Choose {
                title: "Start the node?".into(),
                lines,
                opts: vec![
                    ("Request seed node access".into(), Pick::GoSeed),
                    ("Start with just my picks for now".into(), Pick::Start),
                    ("Cancel".into(), Pick::Cancel),
                ],
                current: None,
                state: sel(0),
            });
            return;
        }
        let scope = chosen;
        let mut lines = vec![match scope.as_str() {
            "full" => "This computer is an approved seed node holding the whole library, audio and video.".to_string(),
            "audio" => "This computer is an approved seed node holding the whole audio library.".to_string(),
            _ => "This computer holds only the speakers and sermons you pick.".to_string(),
        }];
        let need = self.still_needed(&scope);
        if let Some((n, b)) = need {
            lines.push(format!("Still to download: {} ({}).", plural(n, "file", "files"), fmt_bytes(b)));
        }
        if self.disk.1 > 0 {
            lines.push(format!(
                "Free on this drive: {}. The node always leaves {} free for the computer.",
                fmt_bytes(self.disk.0),
                fmt_bytes(self.keep_free())
            ));
        }
        let too_big = need.map(|(_, b)| b > self.room()).unwrap_or(false);
        if too_big {
            lines.push(format!(
                "That will not all fit. It downloads about {} and then pauses downloads, \
                 still sharing everything it has.",
                fmt_bytes(self.room())
            ));
        }
        lines.push(String::new());
        if self.node_is_service() {
            lines.push("The node is installed as a service, so starting it may ask for your password.".into());
        } else {
            lines.push("It runs in the background and keeps going after you leave the menu.".into());
        }
        let empty_picks = scope == "picks" && self.picks.is_empty();
        let opts = if empty_picks {
            vec![
                ("Pick speakers first".to_string(), Pick::GoSpeakers),
                ("Start anyway — only share what is already here".to_string(), Pick::Start),
                ("Cancel".to_string(), Pick::Cancel),
            ]
        } else if too_big || scope != "picks" {
            let change = if scope == "picks" {
                ("Change my picks first".to_string(), Pick::GoPicks)
            } else {
                ("Change what this computer holds first".to_string(), Pick::GoScope)
            };
            let mut v = vec![("Start the node".to_string(), Pick::Start), change, ("Cancel".to_string(), Pick::Cancel)];
            if too_big {
                v.swap(0, 1);
            }
            v
        } else {
            vec![("Start the node".to_string(), Pick::Start), ("Cancel".to_string(), Pick::Cancel)]
        };
        self.overlay = Overlay::Choose(Choose {
            title: "Start the node?".into(),
            lines,
            opts,
            current: None,
            state: sel(0),
        });
    }

    fn start_node(&mut self) {
        if self.node_is_service() {
            self.run_cmd = Some(("Starting the node service".into(), systemctl("start")));
            return;
        }
        match spawn_background() {
            Ok(()) => self.say("Starting… its activity appears on the right. It keeps running after you quit the menu."),
            Err(e) => self.say(format!("Could not start the node: {e}")),
        }
    }

    fn stop_node(&mut self) {
        if self.node_is_service() {
            self.run_cmd = Some(("Stopping the node service".into(), systemctl("stop")));
            return;
        }
        let pid = self.stats.as_ref().and_then(|s| s["node"]["pid"].as_u64());
        match pid {
            Some(p) => match signal_stop(p) {
                Ok(()) => self.say("Stopping the node… Files already downloaded stay."),
                Err(e) => self.say(e),
            },
            None => self.say("This node is an older version that can't be stopped from here — press Ctrl-C in its window."),
        }
    }

    fn restart_node(&mut self) {
        if self.node_is_service() {
            self.run_cmd = Some(("Restarting the node service".into(), systemctl("restart")));
            return;
        }
        let Some(pid) = self.stats.as_ref().and_then(|s| s["node"]["pid"].as_u64()) else {
            self.say("This node can't be restarted from here — press Ctrl-C in its window and start it again.");
            return;
        };
        if let Err(e) = signal_stop(pid) {
            self.say(e);
            return;
        }
        self.say("Restarting the node…");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Wait for the old one to let go of its ports, then start anew.
            for _ in 0..60 {
                if fetch_stats().is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            std::thread::sleep(Duration::from_secs(1));
            let m = match spawn_background() {
                Ok(()) => "The node is starting again.".to_string(),
                Err(e) => format!("Could not start the node again: {e}"),
            };
            let _ = tx.send(Msg::Said(m));
        });
    }

    fn do_pick(&mut self, p: Pick) {
        match p {
            Pick::Start => self.start_node(),
            Pick::GoScope => self.goto(HomeAct::Scope),
            Pick::GoSpeakers => self.goto(HomeAct::Speakers),
            Pick::GoPicks => self.goto(HomeAct::MyPicks),
            Pick::GoSeed => self.goto(HomeAct::Seed),
            Pick::Set(i, v) => {
                let _ = self.apply_setting(SETTINGS[i].key, &v);
            }
            Pick::Custom(i) => {
                self.overlay = Overlay::Edit(Edit { idx: i, value: String::new(), error: None });
            }
            Pick::Cancel => {}
        }
    }

    /// The pick-one box for a setting with presets.
    fn preset_box(&mut self, i: usize) {
        let st = &SETTINGS[i];
        let Kind::Preset(opts) = st.kind else { return };
        let cur = raw_value(&self.settings, st.key);
        let current = opts.iter().position(|(_, v)| *v != CUSTOM && same_value(st.key, v, &cur));
        let custom_now = current.is_none() && opts.iter().any(|(_, v)| *v == CUSTOM);
        let list: Vec<(String, Pick)> = opts
            .iter()
            .map(|(l, v)| {
                if *v == CUSTOM {
                    let lbl = if custom_now {
                        format!("Custom…  (now {})", show_value(&self.settings, st.key))
                    } else {
                        l.to_string()
                    };
                    (lbl, Pick::Custom(i))
                } else {
                    (l.to_string(), Pick::Set(i, v.to_string()))
                }
            })
            .collect();
        let at = current.or(if custom_now { list.iter().position(|(_, p)| *p == Pick::Custom(i)) } else { None });
        self.overlay = Overlay::Choose(Choose {
            title: st.label.into(),
            lines: vec![st.hint.to_string()],
            opts: list,
            current: at,
            state: sel(at.unwrap_or(0)),
        });
    }

    fn apply_setting(&mut self, key: &str, val: &str) -> std::result::Result<(), String> {
        let mut s = config::load_settings();
        let mut notes = Vec::new();
        match cfg::apply(&mut s, key, val, &mut notes) {
            Ok(restart) => {
                config::save_settings(&s).map_err(|e| e.to_string())?;
                self.settings = s;
                let msg = if restart && self.running() {
                    "Saved. Choose Restart the node in the menu for it to take effect.".to_string()
                } else {
                    "Saved.".to_string()
                };
                if key == "keep-free" || key == "dir" {
                    self.disk = system::disk_space(&config::downloads_dir(&self.settings));
                }
                self.say(msg);
                Ok(())
            }
            Err(e) => Err(format!("{e}")),
        }
    }

    // ── keys ──

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.quit = true;
            return;
        }
        // Overlays take every key while open.
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::Edit(mut e) => {
                match k.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let st = &SETTINGS[e.idx];
                        let res = to_config(st.unit, e.value.trim()).and_then(|v| self.apply_setting(st.key, &v));
                        if let Err(err) = res {
                            e.error = Some(err);
                            self.overlay = Overlay::Edit(e);
                        }
                    }
                    KeyCode::Backspace => {
                        e.value.pop();
                        e.error = None;
                        self.overlay = Overlay::Edit(e);
                    }
                    KeyCode::Char(c) => {
                        e.value.push(c);
                        e.error = None;
                        self.overlay = Overlay::Edit(e);
                    }
                    _ => self.overlay = Overlay::Edit(e),
                }
                return;
            }
            Overlay::Email(mut v, _) => {
                match k.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        let e = v.trim().to_string();
                        let ok = e.contains('@') && e.rsplit('@').next().map(|d| d.contains('.')).unwrap_or(false);
                        if !ok || e.contains(' ') {
                            self.overlay = Overlay::Email(v, Some("That doesn't look like an email address — e.g. pastor@church.org".into()));
                        } else {
                            let mut s = config::load_settings();
                            s["seed_request_email"] = serde_json::json!(e);
                            s["seed_requested_at"] = serde_json::json!(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0)
                            );
                            let _ = config::save_settings(&s);
                            self.settings = s;
                            self.sending = true;
                            self.say("Sending the request…");
                            spawn_seed_request(self.tx.clone(), self.node_id.clone(), e);
                        }
                    }
                    KeyCode::Backspace => {
                        v.pop();
                        self.overlay = Overlay::Email(v, None);
                    }
                    KeyCode::Char(c) if !c.is_control() => {
                        v.push(c);
                        self.overlay = Overlay::Email(v, None);
                    }
                    _ => self.overlay = Overlay::Email(v, None),
                }
                return;
            }
            Overlay::Choose(mut c) => {
                let n = c.opts.len();
                let i = c.state.selected().unwrap_or(0).min(n.saturating_sub(1));
                match k.code {
                    KeyCode::Esc | KeyCode::Left => {}
                    KeyCode::Up => {
                        c.state.select(Some(if i == 0 { n - 1 } else { i - 1 }));
                        self.overlay = Overlay::Choose(c);
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        c.state.select(Some((i + 1) % n));
                        self.overlay = Overlay::Choose(c);
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        let p = c.opts[i].1.clone();
                        self.do_pick(p);
                    }
                    _ => self.overlay = Overlay::Choose(c),
                }
                return;
            }
            Overlay::Confirm(c) => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.do_action(c.action),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {}
                    _ => self.overlay = Overlay::Confirm(c),
                }
                return;
            }
            Overlay::None => {}
        }

        let scr = self.screen();
        if scr == Screen::Speakers {
            self.keys_speakers(k);
            return;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                // Esc never quits: someone tapping Esc to "get back" should
                // land on the menu, not out of it. q or Quit leaves.
                if scr != Screen::Home {
                    self.back();
                }
                return;
            }
            KeyCode::Char('q') => {
                self.quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.push(Screen::Help);
                return;
            }
            _ => {}
        }
        match scr {
            Screen::Home => self.keys_home(k),
            Screen::Speaker(i) => self.keys_speaker(k, i),
            Screen::Sermons(i) => self.keys_sermons(k, i),
            Screen::Discover => self.keys_discover(k),
            Screen::MyPicks => self.keys_picks(k),
            Screen::Scope => self.keys_scope(k),
            Screen::Settings => self.keys_settings(k),
            Screen::Dashboard => self.keys_dashboard(k),
            Screen::Seed => self.keys_seed(k),
            Screen::Connections => self.keys_conn(k),
            Screen::Updates => self.keys_updates(k),
            _ => {}
        }
    }

    fn do_action(&mut self, a: Action) {
        match a {
            Action::AddSpeakers(v) => {
                let names: Vec<String> = match &self.catalog {
                    Some(c) => v.iter().map(|&i| c.speakers[i].name.clone()).collect(),
                    None => return,
                };
                for n in &names {
                    if self.picks.speaker(n).is_none() {
                        self.picks.set_speaker(n, false);
                    }
                }
                let note = self.covered_note(false);
                self.save_picks(&format!("Added {} speakers.{note}", names.len()));
            }
            Action::RemoveSpeaker(n) => {
                self.picks.remove_speaker(&n);
                self.save_picks(&format!("{n} removed from your picks — files already downloaded stay."));
            }
        }
    }

    fn home_items(&self) -> Vec<(HomeAct, String, String)> {
        let n_spk = self.catalog.as_ref().map(|c| c.speaker_index().len()).unwrap_or(0);
        let n_srm = self.catalog.as_ref().map(|c| c.sermons.len()).unwrap_or(0);
        let cat_line = if self.catalog.is_some() {
            format!("{} speakers, {} sermons.", fmt_n(n_spk as u64), fmt_n(n_srm as u64))
        } else if let Some(e) = &self.catalog_err {
            format!("The sermon list could not be loaded: {e}")
        } else {
            "Loading the sermon list…".to_string()
        };
        let mut v = vec![
            (HomeAct::Status, "Node status".to_string(),
                "What this node is doing right now: sharing, peers, how much of the library it holds, and how much memory it is using.".to_string()),
            (HomeAct::Connections, "Connections".to_string(),
                "How other nodes reach this one — IPv4 and IPv6, the router port, peers coming in and going out — and a test you can run now.".to_string()),
            (HomeAct::Speakers, "Speakers".to_string(),
                format!("Browse every preacher and download all of one speaker's sermons. {cat_line}")),
            (HomeAct::Discover, "Discover".to_string(),
                "Ten speakers at random — press r for ten more. A good way to find someone new.".to_string()),
            (HomeAct::MyPicks, "My picks".to_string(),
                format!(
                    "What you have chosen to download, and how far it has got. {}, {}.",
                    plural(self.picks.speakers.len(), "speaker", "speakers"),
                    plural(self.picks.sermons.len(), "single sermon", "single sermons")
                )),
            (HomeAct::Scope, "What this node holds".to_string(),
                format!("Only your picks, the whole audio library, or everything. Now: {}.", scope_name(&self.scope()))),
            (HomeAct::Settings, "Settings".to_string(),
                "Upload speed, seeding hours, monthly cap, library folder and sharing options.".to_string()),
        ];
        let (seed_lbl, seed_desc) = if self.granted() {
            (format!("Seed node  {}", self.t.g("✓", "+")),
             "Approved: this machine may hold the whole library. Choose audio or everything, and see how far it has got.".to_string())
        } else {
            match self.seed_word.as_deref() {
                Some("denied") => ("Seed node  (declined)".to_string(),
                    "This machine's request to be a seed node was declined. You can ask again from here.".to_string()),
                Some("pending") => ("Seed node  (waiting)".to_string(),
                    "Your request is waiting for approval on the SermonIndex console. The page updates by itself.".to_string()),
                _ => ("Seed node".to_string(),
                    "Hold and share the WHOLE library, like the desktop app's Seed Node page. Needs approval for this \
                     machine — send the request from here.".to_string()),
            }
        };
        v.insert(5, (HomeAct::Seed, seed_lbl, seed_desc));
        v.push((HomeAct::Dashboard, "Dashboard in a browser".to_string(),
            "The live stats display in any web browser — on this computer, or on a phone, tablet or TV \
             on the same network. Shows the addresses to type.".to_string()));
        let upd = match &self.latest {
            Some(Some(v)) => format!("Updates  {} v{v}", self.t.g("●", "*")),
            _ => "Updates".to_string(),
        };
        let upd_desc = match &self.latest {
            None => format!("Running v{}. Checking for a newer release…", update::current()),
            Some(None) => format!("Running v{} — the newest release.", update::current()),
            Some(Some(v)) => format!("v{v} is available (running v{}). Install it from here.", update::current()),
        };
        v.push((HomeAct::Updates, upd, upd_desc));
        let svc = self.node_is_service();
        if self.running() {
            v.push((HomeAct::Stop, "Stop the node".to_string(),
                "Stop downloading and sharing. Everything downloaded stays on the disk.".to_string()));
            v.push((HomeAct::Restart, "Restart the node".to_string(),
                "Stop and start again — needed after changing the library folder, what it holds, or \
                 the sharing settings.".to_string()));
        } else {
            let how = if svc {
                "It is installed as a service, so it may ask for your password."
            } else {
                "It runs in the background and keeps going after you leave the menu."
            };
            v.push((HomeAct::Start, "Start the node".to_string(),
                format!("Begin downloading what this computer holds and sharing it. Shows what it will \
                         download and whether it fits before anything starts. {how}")));
        }
        v.push((HomeAct::Help, "Help & keys".to_string(), "Every key, and the matching command for scripts.".to_string()));
        v.push((HomeAct::Quit, "Quit".to_string(), "Leave the menu. A running node keeps running.".to_string()));
        v
    }

    fn keys_home(&mut self, k: KeyEvent) {
        let items = self.home_items();
        let n = items.len();
        let i = self.home.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.home.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.home.select(Some((i + 1) % n)),
            KeyCode::Enter | KeyCode::Right => match items[i].0 {
                HomeAct::Status => self.push(Screen::Status),
                HomeAct::Speakers => self.push(Screen::Speakers),
                HomeAct::Discover => {
                    if self.disc.is_empty() {
                        self.shuffle();
                    }
                    self.push(Screen::Discover)
                }
                HomeAct::MyPicks => self.push(Screen::MyPicks),
                HomeAct::Scope => self.goto(HomeAct::Scope),
                HomeAct::Seed => self.goto(HomeAct::Seed),
                HomeAct::Connections => {
                    self.conn_state.select(Some(0));
                    self.push(Screen::Connections)
                }
                HomeAct::Settings => self.push(Screen::Settings),
                HomeAct::Dashboard => {
                    self.dash_state.select(Some(0));
                    self.push(Screen::Dashboard)
                }
                HomeAct::Updates => {
                    self.upd_state.select(Some(0));
                    self.push(Screen::Updates)
                }
                HomeAct::Start => self.start_prompt(),
                HomeAct::Stop => self.stop_node(),
                HomeAct::Restart => self.restart_node(),
                HomeAct::Help => self.push(Screen::Help),
                HomeAct::Quit => self.quit = true,
            },
            _ => {}
        }
    }

    fn keys_speakers(&mut self, k: KeyEvent) {
        let n = self.view.len();
        let i = self.spk.selected().unwrap_or(0);
        match k.code {
            KeyCode::Esc => {
                if self.query.is_empty() {
                    self.back();
                } else {
                    self.query.clear();
                    self.rebuild_view();
                }
            }
            KeyCode::Left if self.query.is_empty() => self.back(),
            KeyCode::Backspace => {
                if self.query.pop().is_some() {
                    self.rebuild_view();
                } else {
                    self.back();
                }
            }
            KeyCode::Tab => {
                self.sort_count = !self.sort_count;
                self.rebuild_view();
            }
            KeyCode::Up if n > 0 => self.spk.select(Some(i.saturating_sub(1))),
            KeyCode::Down if n > 0 => self.spk.select(Some((i + 1).min(n - 1))),
            KeyCode::PageUp if n > 0 => self.spk.select(Some(i.saturating_sub(10))),
            KeyCode::PageDown if n > 0 => self.spk.select(Some((i + 10).min(n - 1))),
            KeyCode::Home if n > 0 => self.spk.select(Some(0)),
            KeyCode::End if n > 0 => self.spk.select(Some(n - 1)),
            KeyCode::Enter | KeyCode::Right if n > 0 => {
                let si = self.view[i.min(n - 1)];
                self.act.select(Some(0));
                self.push(Screen::Speaker(si));
            }
            KeyCode::Char(' ') if n > 0 && self.query.is_empty() => {
                // Space = quick pick (audio). Inside a search it is a space.
                let si = self.view[i.min(n - 1)];
                self.toggle_audio_pick(si);
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.query.push(c);
                self.rebuild_view();
            }
            _ => {}
        }
    }

    fn toggle_audio_pick(&mut self, si: usize) {
        let Some(name) = self.catalog.as_ref().map(|c| c.speakers[si].name.clone()) else { return };
        if self.picks.speaker(&name).is_some() {
            self.picks.remove_speaker(&name);
            self.save_picks(&format!("{name} removed from your picks — files already downloaded stay."));
        } else {
            self.picks.set_speaker(&name, false);
            let note = self.covered_note(false);
            self.save_picks(&format!("{name} added to your picks.{note}"));
        }
    }

    /// A pick the scope already covers adds nothing — say so, rather than
    /// letting someone wait for downloads that will never come.
    fn covered_note(&self, video: bool) -> &'static str {
        match (self.scope().as_str(), video) {
            ("full", _) => " (This node already holds everything.)",
            ("audio", false) => " (This node already holds every audio sermon.)",
            _ => "",
        }
    }

    fn spk_actions(&self, si: usize) -> Vec<(SpkAct, String)> {
        let Some(cat) = &self.catalog else { return vec![(SpkAct::Back, "Back".into())] };
        let sp = &cat.speakers[si];
        let mut v = vec![(
            SpkAct::Audio,
            format!("Download all audio  ({} sermons, {})", fmt_n(sp.audio as u64), fmt_bytes(sp.audio_bytes)),
        )];
        if sp.video > 0 {
            v.push((
                SpkAct::All,
                format!(
                    "Download audio and video  ({} files, {})",
                    fmt_n(sp.sermons.len() as u64),
                    fmt_bytes(sp.all_bytes)
                ),
            ));
        }
        v.push((SpkAct::Choose, "Choose individual sermons…".into()));
        if self.picks.speaker(&sp.name).is_some() {
            v.push((SpkAct::Stop, "Stop downloading this speaker".into()));
        }
        v.push((SpkAct::Back, "Back".into()));
        v
    }

    fn keys_speaker(&mut self, k: KeyEvent, si: usize) {
        let acts = self.spk_actions(si);
        let n = acts.len();
        let i = self.act.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.act.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.act.select(Some((i + 1) % n)),
            KeyCode::Enter | KeyCode::Right => {
                let Some(name) = self.catalog.as_ref().map(|c| c.speakers[si].name.clone()) else { return };
                match acts[i].0 {
                    SpkAct::Audio => {
                        self.picks.set_speaker(&name, false);
                        let note = self.covered_note(false);
                        self.save_picks(&format!("All of {name}'s audio is in your picks.{note}"));
                    }
                    SpkAct::All => {
                        self.picks.set_speaker(&name, true);
                        let note = self.covered_note(true);
                        self.save_picks(&format!("All of {name}'s audio and video is in your picks.{note}"));
                    }
                    SpkAct::Choose => {
                        self.srm.select(Some(0));
                        self.push(Screen::Sermons(si));
                    }
                    SpkAct::Stop => {
                        self.overlay = Overlay::Confirm(Confirm {
                            title: "Stop downloading?".into(),
                            lines: vec![
                                format!("Remove {name} from your picks."),
                                "Nothing is deleted: files already downloaded stay and keep".into(),
                                "being shared. The node just won't fetch any more of them.".into(),
                            ],
                            action: Action::RemoveSpeaker(name),
                        });
                    }
                    SpkAct::Back => self.back(),
                }
            }
            _ => {}
        }
    }

    fn keys_sermons(&mut self, k: KeyEvent, si: usize) {
        let Some(n) = self.catalog.as_ref().map(|c| c.speakers[si].sermons.len()) else { return };
        if n == 0 {
            return;
        }
        let i = self.srm.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.srm.select(Some(i.saturating_sub(1))),
            KeyCode::Down => self.srm.select(Some((i + 1).min(n - 1))),
            KeyCode::PageUp => self.srm.select(Some(i.saturating_sub(10))),
            KeyCode::PageDown => self.srm.select(Some((i + 10).min(n - 1))),
            KeyCode::Home => self.srm.select(Some(0)),
            KeyCode::End => self.srm.select(Some(n - 1)),
            KeyCode::Char(' ') | KeyCode::Enter => {
                let (id, title) = match &self.catalog {
                    Some(c) => {
                        let s = &c.sermons[c.speakers[si].sermons[i]];
                        (s.id.clone(), s.title.clone())
                    }
                    None => return,
                };
                self.picks.toggle_sermon(&id);
                let short = trunc(&title, 40).trim_end().to_string();
                let msg = if self.picks.has_sermon(&id) {
                    format!("Picked “{short}”.")
                } else {
                    format!("Unpicked “{short}”.")
                };
                self.save_picks(&msg);
                self.srm.select(Some((i + 1).min(n - 1)));
            }
            _ => {}
        }
    }

    fn keys_discover(&mut self, k: KeyEvent) {
        let n = self.disc.len() + 1; // row 0 = "surprise me"
        let i = self.disc_state.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.disc_state.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.disc_state.select(Some((i + 1) % n)),
            KeyCode::Char('r') => {
                self.shuffle();
                self.say("Ten more.");
            }
            KeyCode::Enter | KeyCode::Right => {
                if i == 0 {
                    self.surprise();
                } else {
                    self.act.select(Some(0));
                    self.push(Screen::Speaker(self.disc[i - 1]));
                }
            }
            _ => {}
        }
    }

    fn surprise(&mut self) {
        let mut pool: Vec<usize> = match &self.catalog {
            Some(cat) => (0..cat.speakers.len())
                .filter(|&i| cat.speakers[i].audio >= 5 && self.picks.speaker(&cat.speakers[i].name).is_none())
                .collect(),
            None => return,
        };
        let mut chosen = Vec::new();
        while chosen.len() < 5 && !pool.is_empty() {
            let k = (self.rand() as usize) % pool.len();
            chosen.push(pool.swap_remove(k));
        }
        let Some(cat) = &self.catalog else { return };
        let bytes: u64 = chosen.iter().map(|&i| cat.speakers[i].audio_bytes).sum();
        let files: usize = chosen.iter().map(|&i| cat.speakers[i].audio).sum();
        let mut lines: Vec<String> = chosen
            .iter()
            .map(|&i| format!("  {}  ({} sermons)", cat.speakers[i].name, cat.speakers[i].audio))
            .collect();
        lines.push(String::new());
        lines.push(format!("{} audio sermons, {} in all. Add them?", fmt_n(files as u64), fmt_bytes(bytes)));
        self.overlay = Overlay::Confirm(Confirm {
            title: "Five speakers at random".into(),
            lines,
            action: Action::AddSpeakers(chosen),
        });
    }

    fn keys_picks(&mut self, k: KeyEvent) {
        let n = self.picks.speakers.len();
        if n == 0 {
            return;
        }
        let i = self.pk.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.pk.select(Some(i.saturating_sub(1))),
            KeyCode::Down => self.pk.select(Some((i + 1).min(n - 1))),
            KeyCode::Enter | KeyCode::Right => {
                let name = self.picks.speakers[i].name.clone();
                if let Some(si) = self.catalog.as_ref().and_then(|c| c.speaker_index().get(&name).copied()) {
                    self.act.select(Some(0));
                    self.push(Screen::Speaker(si));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                let name = self.picks.speakers[i].name.clone();
                self.overlay = Overlay::Confirm(Confirm {
                    title: "Remove from picks?".into(),
                    lines: vec![
                        format!("Stop downloading {name}."),
                        "Files already downloaded stay and keep being shared.".into(),
                    ],
                    action: Action::RemoveSpeaker(name),
                });
            }
            _ => {}
        }
    }

    fn keys_scope(&mut self, k: KeyEvent) {
        let i = self.scope_state.selected().unwrap_or(0).min(2);
        match k.code {
            KeyCode::Up => self.scope_state.select(Some(if i == 0 { 2 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.scope_state.select(Some((i + 1) % 3)),
            KeyCode::Enter => {
                let v = ["picks", "audio", "full"][i];
                if v != "picks" && !self.granted() {
                    self.goto(HomeAct::Seed);
                    self.say("Holding the whole library needs seed node approval first — request it here.");
                    return;
                }
                if self.apply_setting("scope", v).is_ok() {
                    let mut m = format!("This node will hold {}.", scope_name(v));
                    if v == "picks" && self.picks.is_empty() {
                        m.push_str(" You haven't picked anything yet — use Speakers or Discover.");
                    }

                    if let Some((_, need)) = self.still_needed(v) {
                        if need > self.room() {
                            m.push_str(&format!(
                                " It needs {} more but only {} fits on this drive — downloads will pause when it is full.",
                                fmt_bytes(need),
                                fmt_bytes(self.room())
                            ));
                        }
                    }
                    if self.running() {
                        m.push_str(" Choose Restart the node for it to switch.");
                    }
                    self.say(m);
                }
            }
            _ => {}
        }
    }

    fn keys_settings(&mut self, k: KeyEvent) {
        let n = SETTINGS.len();
        let i = self.set_state.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.set_state.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.set_state.select(Some((i + 1) % n)),
            KeyCode::Enter | KeyCode::Right => {
                let s = &SETTINGS[i];
                match s.kind {
                    Kind::Toggle => {
                        let now = toggle_value(&self.settings, s.key);
                        let _ = self.apply_setting(s.key, if now { "off" } else { "on" });
                    }
                    Kind::Preset(_) => self.preset_box(i),
                    Kind::Text => {
                        self.overlay = Overlay::Edit(Edit { idx: i, value: raw_value(&self.settings, s.key), error: None });
                    }
                }
            }
            _ => {}
        }
    }

    fn seed_actions(&self) -> Vec<(u8, String)> {
        let mut v = Vec::new();
        if self.granted() {
            let cur = self.scope();
            let m = |k: &str| if cur == k { self.t.g("(•) ", "(*) ") } else { "( ) " };
            v.push((10, format!("{}Hold the audio library  ({})", m("audio"), self.lib_label("audio"))));
            v.push((11, format!("{}Hold everything, audio and video  ({})", m("full"), self.lib_label("full"))));
            v.push((12, format!("{}Hold only my picks", m("picks"))));
            if self.running() {
                v.push((4, "Restart the node (needed after changing what it holds)".into()));
            } else {
                v.push((3, "Start the node".into()));
            }
        } else {
            let asked = self.settings.get("seed_requested_at").and_then(|v| v.as_u64()).is_some();
            let label = match self.seed_word.as_deref() {
                Some("denied") => "Ask again",
                Some("pending") => "Send the request again",
                Some("none") => "Request seed node access",
                _ if asked => "Send the request again",
                _ => "Request seed node access",
            };
            v.push((1, label.into()));
            v.push((2, "Check again now".into()));
        }
        v.push((9, "Back".into()));
        v
    }

    fn keys_seed(&mut self, k: KeyEvent) {
        let acts = self.seed_actions();
        let n = acts.len();
        let i = self.seed_state.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.seed_state.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.seed_state.select(Some((i + 1) % n)),
            KeyCode::Enter => match acts[i].0 {
                1 => {
                    if self.sending {
                        return;
                    }
                    let prev = self.settings.get("seed_request_email").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    self.overlay = Overlay::Email(prev, None);
                }
                2 => {
                    self.access = None;
                    self.access_err = false;
                    let tx = self.tx.clone();
                    let id = self.node_id.clone();
                    std::thread::spawn(move || {
                        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
                        let Some(c) = client() else { return };
                        let _ = tx.send(Msg::Access(rt.block_on(crate::heartbeat::seed_status(&c, &id))));
                    });
                    self.say("Asking SermonIndex…");
                }
                3 => self.start_prompt(),
                4 => self.restart_node(),
                10 | 11 | 12 => {
                    let v = match acts[i].0 {
                        10 => "audio",
                        11 => "full",
                        _ => "picks",
                    };
                    if self.apply_setting("scope", v).is_ok() {
                        let mut m = format!("This node will hold {}.", scope_name(v));
                        if let Some((_, need)) = self.still_needed(v) {
                            if need > self.room() {
                                m.push_str(&format!(
                                    " It needs {} more but only {} fits on this drive — downloads pause when it is full.",
                                    fmt_bytes(need),
                                    fmt_bytes(self.room())
                                ));
                            }
                        }
                        if self.running() {
                            m.push_str(" Choose Restart the node for it to switch.");
                        }
                        self.say(m);
                    }
                }
                _ => self.back(),
            },
            _ => {}
        }
    }

    fn probe(&self) -> Value {
        self.stats.as_ref().map(|s| s["node"]["probe"].clone()).unwrap_or(Value::Null)
    }

    fn probe_at(&self) -> u64 {
        self.probe()["at"].as_u64().unwrap_or(0)
    }

    /// One sentence for the footer when a test comes back.
    fn probe_summary(&self) -> String {
        let p = self.probe();
        if p["ran"].as_bool() != Some(true) {
            return "The test server couldn't be reached — try again in a minute.".into();
        }
        let v4 = p["v4"].as_bool() == Some(true);
        let v6 = p["v6"].as_bool() == Some(true);
        let seen = self.stats.as_ref().and_then(|s| s["node"]["v6_confirmed"].as_bool()).unwrap_or(false);
        match (v4, v6 || seen) {
            (true, true) => "Test done: other nodes can reach you over IPv4 and IPv6.".into(),
            (true, false) => "Test done: other nodes can reach you over IPv4.".into(),
            (false, true) => "Test done: IPv4 is closed, but other nodes can reach you over IPv6.".into(),
            (false, false) => "Test done: other nodes can't connect in yet — see the steps on the Connections page.".into(),
        }
    }

    fn conn_actions(&self) -> Vec<(u8, String)> {
        let natpmp = config::natpmp_enabled(&self.settings);
        vec![
            (1, if self.testing.is_some() { "Testing…".into() } else { "Test my connection now".into() }),
            (2, format!("Open my router port automatically: {}", if natpmp { "On" } else { "Off" })),
            (9, "Back".into()),
        ]
    }

    fn keys_conn(&mut self, k: KeyEvent) {
        let acts = self.conn_actions();
        let n = acts.len();
        let i = self.conn_state.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.conn_state.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.conn_state.select(Some((i + 1) % n)),
            KeyCode::Enter => match acts[i].0 {
                1 => {
                    if self.testing.is_some() {
                        return;
                    }
                    if !self.running() {
                        self.say("The test needs the node running — choose Start the node first.");
                        return;
                    }
                    let _ = std::fs::create_dir_all(config::data_dir());
                    match std::fs::write(config::data_dir().join("test-connection"), b"") {
                        Ok(()) => {
                            self.testing = Some((Instant::now(), self.probe_at()));
                            self.say("Testing… a server on the internet is trying to connect to this node.");
                        }
                        Err(e) => self.say(format!("Could not start the test: {e}")),
                    }
                }
                2 => {
                    let on = config::natpmp_enabled(&self.settings);
                    let _ = self.apply_setting("natpmp", if on { "off" } else { "on" });
                }
                _ => self.back(),
            },
            _ => {}
        }
    }

    /// Everything the desktop app's Connections panel shows, as text.
    fn draw_conn(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let none = Color::Reset;
        let kv = |k: &str, v: String, c: Color| {
            Line::from(vec![Span::styled(format!("{k:<16}"), Style::new().fg(t.muted)), Span::styled(v, Style::new().fg(c))])
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ago = |at: u64| -> String {
            let d = now.saturating_sub(at);
            match d {
                0..=89 => "just now".into(),
                90..=5399 => format!("{} min ago", d / 60),
                5400..=172_799 => format!("{} h ago", d / 3600),
                _ => format!("{} days ago", d / 86_400),
            }
        };
        let mut lines: Vec<Line> = Vec::new();
        let v6addr = crate::net::global_ipv6().map(|a| a.to_string());
        match &self.stats {
            None => {
                lines.push(Line::styled("The node isn't running, so there is nothing to test yet.", Style::new().fg(t.gold)));
                lines.push(Line::raw("Choose Start the node, then come back here."));
                lines.push(Line::raw(""));
                lines.push(kv("IPv6 address", v6addr.clone().unwrap_or_else(|| "none on this computer".into()), none));
                lines.push(kv("This computer", self.lan.clone().unwrap_or_else(|| "—".into()), none));
            }
            Some(s) => {
                let n = &s["node"];
                let u = |k: &str| n[k].as_u64().unwrap_or(0);
                let (what, col) = match n["category"].as_str().unwrap_or("") {
                    "seed" => ("Seed node — approved, and other nodes can connect in", t.green),
                    "node" => ("Node — other nodes can connect in", t.green),
                    _ if n["reachable"].as_str() == Some("unknown") => ("Not tested yet", t.muted),
                    _ => ("Peer — it connects out to others, but they can't connect in", t.gold),
                };
                lines.push(kv("This node is", what.into(), col));
                let port = u("port");
                let local = u("local_port");
                let port_txt = if local != 0 && local != port {
                    format!("{port} (TCP and UDP) — listening locally on {local}")
                } else {
                    format!("{port} (TCP and UDP)")
                };
                lines.push(kv("Port", port_txt, none));
                lines.push(kv("This computer", self.lan.clone().unwrap_or_else(|| "—".into()), none));
                lines.push(Line::raw(""));
                let p = &n["probe"];
                let tested = p["at"].as_u64().unwrap_or(0);
                let when = if tested > 0 { format!("  (tested {})", ago(tested)) } else { String::new() };
                let (v4, c4) = match (p["ran"].as_bool(), p["v4"].as_bool()) {
                    (Some(true), Some(true)) => (format!("open ✓{when}"), t.green),
                    (Some(true), Some(false)) => (format!("closed{when}"), t.gold),
                    (Some(false), _) => (format!("test server unreachable{when}"), t.red),
                    _ => ("not tested yet".into(), t.muted),
                };
                lines.push(kv("IPv4", v4, c4));
                lines.push(kv("IPv6 address", v6addr.clone().unwrap_or_else(|| "none — IPv6 isn't available on this connection".into()), none));
                let (v6t, c6) = match (p["ran"].as_bool(), p["v6"].as_bool()) {
                    (Some(true), Some(true)) => ("open ✓".to_string(), t.green),
                    (Some(true), Some(false)) => ("the test couldn't get through (its IPv6 reach is limited)".to_string(), t.muted),
                    _ if v6addr.is_none() => ("—".into(), t.muted),
                    _ => ("not tested yet".into(), t.muted),
                };
                lines.push(kv("IPv6 test", v6t, c6));
                if let Some(r) = p["v6_router"].as_str() {
                    lines.push(kv("IPv6 router", r.to_string(), if r.starts_with("the router opened") { t.green } else { t.muted }));
                }
                let seen_at = u("v6_inbound_at");
                let (seen, cs) = if n["v6_confirmed"].as_bool() == Some(true) {
                    (format!("a peer connected IN over IPv6 {} ✓ — reachable", if seen_at > 0 { ago(seen_at) } else { "recently".into() }), t.green)
                } else if n["v6_inbound_seen"].as_bool() == Some(true) {
                    (format!("last IPv6 peer connected in {}", ago(seen_at)), t.gold)
                } else {
                    ("no peer has connected in over IPv6 yet".into(), t.muted)
                };
                lines.push(kv("IPv6 proof", seen, cs));
                lines.push(kv("Router", n["natpmp"].as_str().filter(|x| !x.is_empty()).unwrap_or("—").to_string(), none));
                lines.push(Line::raw(""));
                lines.push(kv(
                    "Peers",
                    format!(
                        "{} connected · {} came to you · {} you reached · most at once coming in: {}",
                        u("peers"),
                        u("peers_in"),
                        u("peers_out"),
                        u("peers_in_peak")
                    ),
                    if u("peers") > 0 { t.green } else { none },
                ));
                lines.push(kv(
                    "Finding peers",
                    format!(
                        "DHT {} · sharing {}",
                        if config::dht_enabled(&self.settings) { "on" } else { "off" },
                        if config::p2p_enabled(&self.settings) { "on" } else { "off" }
                    ),
                    none,
                ));
                lines.push(Line::raw(""));
                let reachable = matches!(n["category"].as_str(), Some("seed") | Some("node"));
                if reachable {
                    lines.push(Line::styled("Other nodes can connect to this one. Nothing to do.", Style::new().fg(t.green)));
                } else {
                    lines.push(Line::styled("To let other nodes connect in:", Style::new().fg(t.gold).add_modifier(Modifier::BOLD)));
                    lines.push(Line::raw(format!(
                        "1. In your router, forward TCP and UDP port {port} to this computer{}.",
                        self.lan.as_deref().map(|ip| format!(" ({ip})")).unwrap_or_default()
                    )));
                    if v6addr.is_some() {
                        lines.push(Line::raw(format!(
                            "2. For IPv6: in the router's IPv6 firewall, allow incoming TCP port {port} to this computer. \
                             (No forwarding needed — IPv6 addresses are already public.)"
                        )));
                    }
                    lines.push(Line::raw("Then choose Test my connection now. The node still shares with every peer it reaches either way."));
                }
            }
        }
        let inner_w = area.width.saturating_sub(4) as usize;
        let rows: usize = lines
            .iter()
            .map(|l| wrap_count(&l.spans.iter().map(|s| s.content.as_ref()).collect::<String>(), inner_w))
            .sum();
        let n_act = self.conn_actions().len() as u16;
        let h = (rows as u16 + 2).min(area.height.saturating_sub(n_act + 2).max(3));
        let [info, list_area] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(area);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(t.block("Connections").padding(ratatui::widgets::Padding::horizontal(1))),
            info,
        );
        let items: Vec<ListItem> =
            self.conn_actions().into_iter().map(|(_, l)| ListItem::new(format!(" {l}"))).collect();
        f.render_stateful_widget(
            List::new(items).block(t.block("")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.conn_state,
        );
    }

    fn dash_actions(&self) -> Vec<(u8, String)> {
        vec![
            (1, "Open the dashboard in this computer's browser".into()),
            (2, "Open the full-screen display (for a small screen)".into()),
            (3, "Back".into()),
        ]
    }

    fn keys_dashboard(&mut self, k: KeyEvent) {
        let acts = self.dash_actions();
        let n = acts.len();
        let i = self.dash_state.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.dash_state.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.dash_state.select(Some((i + 1) % n)),
            KeyCode::Enter => {
                let url = match acts[i].0 {
                    1 => dash_url("localhost"),
                    2 => format!("{}?view=compact", dash_url("localhost")),
                    _ => return self.back(),
                };
                if !self.running() {
                    self.say("The dashboard only answers while the node runs — choose Start the node first.");
                    return;
                }
                match open_url(&url) {
                    Ok(()) => self.say(format!("Opening {url}")),
                    Err(e) => self.say(e),
                }
            }
            _ => {}
        }
    }

    fn upd_actions(&self) -> Vec<(u8, String)> {
        let mut v = Vec::new();
        if let Some(Some(ver)) = &self.latest {
            v.push((1, format!("Install v{ver} now")));
        }
        v.push((2, "Check again".into()));
        v.push((3, "Back".into()));
        v
    }

    fn keys_updates(&mut self, k: KeyEvent) {
        let acts = self.upd_actions();
        let n = acts.len();
        let i = self.upd_state.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Up => self.upd_state.select(Some(if i == 0 { n - 1 } else { i - 1 })),
            KeyCode::Down | KeyCode::Tab => self.upd_state.select(Some((i + 1) % n)),
            KeyCode::Enter => match acts[i].0 {
                1 => {
                    self.after = After::Upgrade;
                    self.quit = true;
                }
                2 => {
                    self.latest = None;
                    spawn_update_check(self.tx.clone());
                }
                _ => self.back(),
            },
            _ => {}
        }
    }

    // ── drawing ──

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let t = self.t;
        // The full wordmark when there is room for it; a one-line name on a
        // small window or the Linux console.
        let logo = !t.ascii && area.width >= 60 && area.height >= 16;
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(if logo { 5 } else { 1 }),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .areas(area);
        if logo {
            let [l, r] = Layout::horizontal([Constraint::Length(LOGO_W), Constraint::Min(10)]).areas(head);
            f.render_widget(Paragraph::new(logo_lines(&t)), l);
            let side = vec![
                Line::raw(""),
                Line::styled(
                    format!("  Node Software  v{}", update::current()),
                    Style::new().fg(t.olive).add_modifier(Modifier::BOLD),
                ),
                self.status_line(false),
            ];
            f.render_widget(Paragraph::new(side), r);
        } else {
            f.render_widget(Paragraph::new(self.status_line(true)), head);
        }

        // On a roomy terminal the menu stays on the left the whole time and
        // everything opens in the panel beside it; on a small one each screen
        // takes the whole window, as before.
        let side = body.width >= 84 && body.height >= 14;
        let body = if side {
            let [nav, main] = Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).areas(body);
            self.draw_nav(f, nav);
            main
        } else {
            body
        };
        match self.screen() {
            Screen::Home if side => self.draw_live(f, body),
            Screen::Home => self.draw_home(f, body),
            Screen::Status => self.draw_status(f, body),
            Screen::Speakers => self.draw_speakers(f, body),
            Screen::Speaker(i) => self.draw_speaker(f, body, i),
            Screen::Sermons(i) => self.draw_sermons(f, body, i),
            Screen::Discover => self.draw_discover(f, body),
            Screen::MyPicks => self.draw_picks(f, body),
            Screen::Scope => self.draw_scope(f, body),
            Screen::Settings => self.draw_settings(f, body),
            Screen::Dashboard => self.draw_dashboard(f, body),
            Screen::Seed => self.draw_seed(f, body),
            Screen::Connections => self.draw_conn(f, body),
            Screen::Updates => self.draw_updates(f, body),
            Screen::Help => self.draw_help(f, body),
        }

        // Footer: a message if there is a fresh one, otherwise the keys.
        let fresh = self.toast.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(8));
        let footer = match fresh {
            Some((m, _)) => Line::from(Span::styled(
                format!(" {m}"),
                Style::new().fg(t.gold).add_modifier(Modifier::BOLD),
            )),
            None => Line::from(Span::styled(format!(" {}", self.hints()), Style::new().fg(t.muted))),
        };
        f.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), foot);

        match &self.overlay {
            Overlay::None => {}
            Overlay::Edit(e) => self.draw_edit(f, area, e),
            Overlay::Confirm(c) => self.draw_confirm(f, area, c),
            Overlay::Email(v, err) => self.draw_email(f, area, v, err.as_deref()),
            Overlay::Choose(_) => {
                if let Overlay::Choose(mut c) = std::mem::replace(&mut self.overlay, Overlay::None) {
                    self.draw_choose(f, area, &mut c);
                    self.overlay = Overlay::Choose(c);
                }
            }
        }
    }

    fn hints(&self) -> &'static str {
        match self.screen() {
            Screen::Home => "↑↓ move   Enter open   ? help   q quit",
            Screen::Speakers => "type to search   Tab sort   Enter open   Space pick   Esc back",
            Screen::Sermons(_) => "Space pick/unpick   PgUp/PgDn page   Esc back",
            Screen::Discover => "Enter open   r ten more   Esc back",
            Screen::MyPicks => "Enter open   d remove   Esc back",
            Screen::Settings => "Enter change   Esc back",
            _ => "↑↓ move   Enter choose   Esc back   q quit",
        }
    }

    fn status_line(&self, with_name: bool) -> Line<'static> {
        let t = self.t;
        let mut v: Vec<Span<'static>> = vec![Span::raw(" ")];
        if with_name {
            let pill = if t.plain {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(t.olive_dk).bg(t.cream).add_modifier(Modifier::BOLD)
            };
            v.push(Span::styled(" sermon ", pill));
            v.push(Span::styled("index ", Style::new().fg(t.cream).add_modifier(Modifier::BOLD)));
            v.push(Span::styled(format!(" v{}   ", update::current()), Style::new().fg(t.olive)));
        } else {
            v.push(Span::raw(" "));
        }
        match &self.stats {
            None => v.push(Span::styled(format!("{} not running here", t.g("○", "o")), Style::new().fg(t.muted))),
            Some(s) => {
                let n = &s["node"];
                let peers = n["peers"].as_u64().unwrap_or(0);
                let (txt, col) = if n["quiet"].as_bool() == Some(true) {
                    ("quiet hours".to_string(), t.gold)
                } else if n["available"].as_bool() == Some(false) {
                    ("starting".to_string(), t.gold)
                } else {
                    (format!("seeding · {peers} peers"), t.green)
                };
                v.push(Span::styled(
                    format!("{} {txt}", t.g("●", "*")),
                    Style::new().fg(col).add_modifier(Modifier::BOLD),
                ));
                let held = n["held"].as_u64().unwrap_or(0);
                let total = n["catalog"].as_u64().unwrap_or(0);
                v.push(Span::styled(
                    format!("   holding {} of {}", fmt_n(held), fmt_n(total)),
                    Style::new().fg(t.muted),
                ));
            }
        }
        if self.disk.1 > 0 {
            let col = if self.disk.0 <= self.keep_free() {
                t.red
            } else if self.room() < 20 * 1024 * 1024 * 1024 {
                t.gold
            } else {
                t.muted
            };
            v.push(Span::styled(format!("   {} free", fmt_bytes(self.disk.0)), Style::new().fg(col)));
        }
        if let Some(Some(ver)) = &self.latest {
            v.push(Span::styled(
                format!("   update v{ver}"),
                Style::new().fg(t.gold).add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(v)
    }

    fn draw_home(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let items = self.home_items();
        let i = self.home.selected().unwrap_or(0).min(items.len() - 1);
        self.home.select(Some(i));
        let list: Vec<ListItem> = items
            .iter()
            .map(|(a, l, _)| {
                let st = if *a == HomeAct::Updates && matches!(self.latest, Some(Some(_))) {
                    Style::new().fg(t.gold)
                } else {
                    Style::new()
                };
                ListItem::new(Line::from(Span::styled(format!(" {l}"), st)))
            })
            .collect();
        let wide = area.width >= 76;
        let (left, right) = if wide {
            let [l, r] = Layout::horizontal([Constraint::Length(32), Constraint::Min(20)]).areas(area);
            (l, r)
        } else {
            let [l, r] = Layout::vertical([Constraint::Min(6), Constraint::Length(5)]).areas(area);
            (l, r)
        };
        f.render_stateful_widget(
            List::new(list).block(t.block("Menu")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            left,
            &mut self.home,
        );
        let mut text = vec![];
        if wide {
            text.push(Line::from(Span::styled(
                items[i].1.trim().to_string(),
                Style::new().fg(t.gold).add_modifier(Modifier::BOLD),
            )));
            text.push(Line::raw(""));
        }
        text.push(Line::raw(items[i].2.clone()));
        if wide {
            text.push(Line::raw(""));
            text.extend(self.home_facts());
        }
        f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(t.block("").padding(ratatui::widgets::Padding::horizontal(1))),
            right,
        );
    }

    /// The few facts worth having on the first screen.
    fn home_facts(&self) -> Vec<Line<'static>> {
        let t = self.t;
        let kv = |k: &str, v: String| {
            Line::from(vec![Span::styled(format!("{k:<12}"), Style::new().fg(t.muted)), Span::raw(v)])
        };
        let scope = self.scope();
        let holds = if (scope == "audio" || scope == "full") && !self.granted() {
            format!("{} — waiting for seed node approval (picks until then)", scope_name(&scope))
        } else if scope == "picks" && self.picks.is_empty() {
            "nothing yet — pick speakers, or ask to be a seed node".to_string()
        } else {
            scope_name(&scope).to_string()
        };
        let mut v = vec![kv("Holds", holds)];
        v.push(kv("Library", tidy_path(&config::downloads_dir(&self.settings))));
        v.push(kv("On disk", format!("{} sermons", fmt_n(self.held.len() as u64))));
        v.push(kv("Disk space", self.disk_words()));
        if let Some((n, b)) = self.still_needed(&self.effective_scope()) {
            let fits = if b > self.room() { "  — more than fits" } else { "" };
            v.push(kv("To download", format!("{} ({}){fits}", plural(n, "file", "files"), fmt_bytes(b))));
        }
        v.push(kv("Dashboard", dash_url("localhost")));
        if let Some(c) = &self.catalog {
            let src = if c.unverified { " (unverified local copy)" } else { "" };
            v.push(kv("Catalogue", format!("{} sermons{src}", fmt_n(c.sermons.len() as u64))));
        }
        v
    }

    fn draw_status(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let kv = |k: &str, v: String, c: Color| {
            Line::from(vec![
                Span::styled(format!("  {k:<18}"), Style::new().fg(t.muted)),
                Span::styled(v, Style::new().fg(c)),
            ])
        };
        let none = Color::Reset;
        let mut lines: Vec<Line> = Vec::new();
        match &self.stats {
            None => {
                lines.push(Line::raw(""));
                lines.push(Line::styled("  No node is running on this machine.", Style::new().fg(t.gold)));
                lines.push(Line::raw(""));
                lines.push(Line::raw("  Choose Start the node in the menu to begin."));
            }
            Some(s) => {
                let n = &s["node"];
                let u = |k: &str| n[k].as_u64().unwrap_or(0);
                let peers = format!(
                    "{} connected  ({} came to you, {} you reached)",
                    u("peers"),
                    u("peers_in"),
                    u("peers_out")
                );
                lines.push(kv("Peers", peers, if u("peers") > 0 { t.green } else { none }));
                lines.push(kv(
                    "Library held",
                    format!(
                        "{} of {} sermons  ({} of the whole collection)",
                        fmt_n(u("held")),
                        fmt_n(u("catalog")),
                        fmt_pct(n["coverage_pct"].as_f64().unwrap_or(0.0))
                    ),
                    none,
                ));
                let st = u("scope_total");
                let goal = match n["scope"].as_str().unwrap_or("") {
                    "audio" | "full" if n["gated"].as_bool() == Some(true) => "your picks (the library waits for seed approval)",
                    "audio" => "the audio library",
                    "full" => "the whole library",
                    _ => "your picks",
                };
                lines.push(kv(
                    "Downloading",
                    if st == 0 {
                        format!("{goal} — nothing chosen yet")
                    } else {
                        format!(
                            "{goal}: {} of {} done  ({:.1}%)",
                            fmt_n(u("scope_held")),
                            fmt_n(st),
                            n["scope_pct"].as_f64().unwrap_or(0.0)
                        )
                    },
                    none,
                ));
                lines.push(kv("On disk", fmt_bytes(u("storage_bytes")), none));
                lines.push(kv("Shared so far", fmt_bytes(u("uploaded_bytes")), none));
                let src = n["source_mode"].as_str().unwrap_or("");
                let (name, _) = config::source_mode_names(src);
                lines.push(kv(
                    "Content source",
                    if src.is_empty() { "waiting for the console".into() } else { name.to_string() },
                    none,
                ));
                lines.push(kv("Reachable", n["reachable"].as_str().unwrap_or("unknown").to_string(), none));
                if n["quiet"].as_bool() == Some(true) {
                    lines.push(kv("Quiet hours", "on — sharing paused".into(), t.gold));
                }
                if n["disk_full"].as_bool() == Some(true) {
                    lines.push(kv("Disk", "full — downloads paused".into(), t.red));
                }
                let up = u("uptime_s");
                lines.push(kv("Running for", format!("{}h {}m", up / 3600, (up % 3600) / 60), none));
                if let Some(v) = n["update_available"].as_str() {
                    lines.push(kv("Update", format!("v{v} available — see Updates"), t.gold));
                }
                let sy = &s["system"];
                if let (Some(used), Some(total)) = (sy["mem_used"].as_u64(), sy["mem_total"].as_u64()) {
                    lines.push(kv("Machine memory", format!("{} of {}", fmt_bytes(used), fmt_bytes(total)), none));
                }
            }
        }
        if let Some(m) = system::service_memory() {
            let cap = m.max.or(m.high).map(|c| format!("  (limit {})", fmt_bytes(c))).unwrap_or_default();
            lines.push(kv(
                "Node memory",
                format!("{} in use + {} disk cache{cap}", fmt_bytes(m.anon), fmt_bytes(m.file)),
                none,
            ));
            lines.push(Line::styled(
                "                    Disk cache is handed back whenever anything else needs it.",
                Style::new().fg(t.muted),
            ));
        }
        lines.push(kv("Disk space", self.disk_words(), if self.room() == 0 { t.red } else { none }));
        let h = (lines.len() as u16 + 2).min(area.height);
        let [top, rest] = Layout::vertical([Constraint::Length(h), Constraint::Min(0)]).areas(area);
        f.render_widget(Paragraph::new(lines).block(t.block("Node status")), top);
        if rest.height >= 4 {
            self.draw_log(f, rest);
        }
    }

    /// The menu, always on the left on a wide terminal. When a section is
    /// open its row stays marked, so you can see where you are.
    fn draw_nav(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let items = self.home_items();
        let i = self.home.selected().unwrap_or(0).min(items.len() - 1);
        self.home.select(Some(i));
        let focus = self.screen() == Screen::Home;
        let list: Vec<ListItem> = items
            .iter()
            .map(|(a, l, _)| {
                let st = match a {
                    HomeAct::Updates if matches!(self.latest, Some(Some(_))) => Style::new().fg(t.gold),
                    HomeAct::Start => Style::new().fg(t.green).add_modifier(Modifier::BOLD),
                    _ => Style::new(),
                };
                ListItem::new(Line::from(Span::styled(format!(" {l}"), st)))
            })
            .collect();
        let hl = if focus { t.hl() } else { Style::new().fg(t.gold).add_modifier(Modifier::BOLD) };
        f.render_stateful_widget(
            List::new(list).block(t.block("Menu")).highlight_style(hl).highlight_symbol(t.g("▸", ">")),
            area,
            &mut self.home,
        );
    }

    /// The panel beside the menu when nothing is open: what the highlighted
    /// item does, the facts that matter, and the node's own activity.
    fn draw_live(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let items = self.home_items();
        let i = self.home.selected().unwrap_or(0).min(items.len() - 1);
        let mut text = vec![Line::raw(items[i].2.clone()), Line::raw("")];
        text.extend(self.home_facts());
        let inner_w = area.width.saturating_sub(4) as usize;
        let h = (wrap_count(&items[i].2, inner_w) as u16 + 1 + self.home_facts().len() as u16 + 2)
            .min(area.height.saturating_sub(4).max(3));
        let [top, rest] = Layout::vertical([Constraint::Length(h), Constraint::Min(0)]).areas(area);
        f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(t.block(items[i].1.trim()).padding(ratatui::widgets::Padding::horizontal(1))),
            top,
        );
        if rest.height >= 3 {
            self.draw_log(f, rest);
        }
    }

    /// The last lines the node printed, newest at the bottom.
    fn draw_log(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let rows = area.height.saturating_sub(2) as usize;
        let w = area.width.saturating_sub(3) as usize;
        let title = if self.running() { "Node activity · running" } else { "Node activity · stopped" };
        let mut lines: Vec<Line> = self
            .log
            .lines
            .iter()
            .rev()
            .take(rows)
            .rev()
            .map(|l| {
                let low = l.to_lowercase();
                let col = if low.contains("error") || low.contains("crashed") || l.contains("FULL") {
                    t.red
                } else if l.starts_with("[disk]") || l.starts_with("[retry]") || l.starts_with("[node]") {
                    t.gold
                } else if l.starts_with(' ') {
                    t.muted
                } else {
                    Color::Reset
                };
                Line::styled(format!(" {}", trunc(l, w).trim_end()), Style::new().fg(col))
            })
            .collect();
        if lines.is_empty() {
            lines.push(Line::styled(
                if self.running() {
                    " Waiting for the node to say something…"
                } else {
                    " Nothing yet. Choose Start the node and its progress shows here."
                },
                Style::new().fg(t.muted),
            ));
        }
        f.render_widget(Paragraph::new(lines).block(t.block(title)), area);
    }

    fn catalog_missing(&self, f: &mut Frame, area: Rect, title: &str) -> bool {
        if self.catalog.is_some() {
            return false;
        }
        let t = self.t;
        let msg = match &self.catalog_err {
            Some(e) => format!(
                "\n  The sermon list could not be loaded.\n\n  {e}\n\n  Check the internet connection and open this screen again."
            ),
            None => "\n  Loading the sermon list…".into(),
        };
        f.render_widget(Paragraph::new(msg).wrap(Wrap { trim: false }).block(t.block(title)), area);
        true
    }

    fn draw_speakers(&mut self, f: &mut Frame, area: Rect) {
        if self.catalog_missing(f, area, "Speakers") {
            return;
        }
        let t = self.t;
        let [top, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
        let sort = if self.sort_count { "most sermons" } else { "A–Z" };
        let search = Line::from(vec![
            Span::styled(" Search: ", Style::new().fg(t.muted)),
            Span::styled(
                if self.query.is_empty() { "type a name".to_string() } else { format!("{}{}", self.query, t.g("▏", "_")) },
                if self.query.is_empty() {
                    Style::new().fg(t.muted)
                } else {
                    Style::new().fg(t.gold).add_modifier(Modifier::BOLD)
                },
            ),
            Span::styled(
                format!("    sorted by {sort}    {} speakers", fmt_n(self.view.len() as u64)),
                Style::new().fg(t.muted),
            ),
        ]);
        f.render_widget(Paragraph::new(search), top);

        let Some(cat) = &self.catalog else { return };
        let name_w = (list_area.width as usize).saturating_sub(40).clamp(12, 44);
        let items: Vec<ListItem> = self
            .view
            .iter()
            .map(|&si| {
                let sp = &cat.speakers[si];
                let held = sp.sermons.iter().filter(|&&k| self.held.contains(&cat.sermons[k].id)).count();
                let mark = match self.picks.speaker(&sp.name) {
                    Some(p) if p.video => Span::styled(format!(" {} +video", t.g("✓", "+")), Style::new().fg(t.green)),
                    Some(_) => Span::styled(format!(" {} picked", t.g("✓", "+")), Style::new().fg(t.green)),
                    None if held > 0 => Span::styled(format!(" {} held", fmt_n(held as u64)), Style::new().fg(t.muted)),
                    None => Span::raw(""),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {} ", trunc(&sp.name, name_w))),
                    Span::styled(format!("{:>6} ", fmt_n(sp.sermons.len() as u64)), Style::new().fg(t.olive)),
                    Span::styled(format!("{:>9}", fmt_bytes(sp.audio_bytes)), Style::new().fg(t.muted)),
                    mark,
                ]))
            })
            .collect();
        let title = format!("Speakers · {} sermons", fmt_n(cat.sermons.len() as u64));
        f.render_stateful_widget(
            List::new(items).block(t.block(&title)).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.spk,
        );
    }

    fn draw_speaker(&mut self, f: &mut Frame, area: Rect, si: usize) {
        if self.catalog_missing(f, area, "Speaker") {
            return;
        }
        let t = self.t;
        let acts = self.spk_actions(si);
        let (held, total) = self.held_of(si);
        let Some(cat) = &self.catalog else { return };
        let sp = &cat.speakers[si];
        let [info, list_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(3)]).areas(area);
        let (pick, picked) = match self.picks.speaker(&sp.name) {
            Some(p) if p.video => ("In your picks: audio and video.", true),
            Some(_) => ("In your picks: audio.", true),
            None => ("Not in your picks.", false),
        };
        let pct = if total > 0 { held as f64 * 100.0 / total as f64 } else { 0.0 };
        let mut first = vec![
            Span::styled(format!(" {} audio", fmt_n(sp.audio as u64)), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" ({})", fmt_bytes(sp.audio_bytes)), Style::new().fg(t.muted)),
        ];
        if sp.video > 0 {
            first.push(Span::styled(
                format!("    {} video", fmt_n(sp.video as u64)),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            first.push(Span::styled(
                format!(" ({})", fmt_bytes(sp.all_bytes.saturating_sub(sp.audio_bytes))),
                Style::new().fg(t.muted),
            ));
        }
        let mut lines = vec![
            Line::from(first),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" On this machine  ", Style::new().fg(t.muted)),
                Span::styled(bar(&t, pct, 20), Style::new().fg(t.gold)),
                Span::raw(format!("  {} of {}", fmt_n(held as u64), fmt_n(total as u64))),
            ]),
            Line::raw(""),
            Line::styled(format!(" {pick}"), Style::new().fg(if picked { t.green } else { t.muted })),
        ];
        if self.disk.1 > 0 {
            let room = self.room();
            let (txt, col) = if sp.audio_bytes > room {
                (format!(" Room for {} more on this drive — not enough for all of the audio.", fmt_bytes(room)), t.red)
            } else {
                (format!(" Room for {} more on this drive.", fmt_bytes(room)), t.muted)
            };
            lines.push(Line::styled(txt, Style::new().fg(col)));
        }
        if cat.unverified {
            lines.push(Line::styled(" (unverified local catalogue)", Style::new().fg(t.red)));
        }
        let name = sp.name.clone();
        f.render_widget(Paragraph::new(lines).block(t.block(&name)), info);
        let items: Vec<ListItem> = acts.iter().map(|(_, l)| ListItem::new(format!(" {l}"))).collect();
        f.render_stateful_widget(
            List::new(items)
                .block(t.block("What would you like to do?"))
                .highlight_style(t.hl())
                .highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.act,
        );
    }

    fn draw_sermons(&mut self, f: &mut Frame, area: Rect, si: usize) {
        if self.catalog_missing(f, area, "Sermons") {
            return;
        }
        let t = self.t;
        let Some(cat) = &self.catalog else { return };
        let sp = &cat.speakers[si];
        let whole = self.picks.speaker(&sp.name).cloned();
        let title_w = (area.width as usize).saturating_sub(34).clamp(16, 90);
        let items: Vec<ListItem> = sp
            .sermons
            .iter()
            .map(|&k| {
                let s = &cat.sermons[k];
                let covered = whole.as_ref().map(|w| w.video || !s.video).unwrap_or(false);
                let picked = self.picks.has_sermon(&s.id);
                let box_ = if picked {
                    Span::styled(t.g("[✓] ", "[x] "), Style::new().fg(t.green).add_modifier(Modifier::BOLD))
                } else if covered {
                    Span::styled(t.g("(•) ", "(*) "), Style::new().fg(t.green))
                } else {
                    Span::styled("[ ] ", Style::new().fg(t.muted))
                };
                let kind = if s.video { "video" } else { "audio" };
                let mins = if s.secs > 0 { format!("{:>3}m", s.secs / 60) } else { "    ".into() };
                let held = if self.held.contains(&s.id) {
                    Span::styled(format!(" {}", t.g("✓ held", "held")), Style::new().fg(t.olive))
                } else {
                    Span::raw("")
                };
                ListItem::new(Line::from(vec![
                    Span::raw(" "),
                    box_,
                    Span::raw(format!("{} ", trunc(if s.title.is_empty() { &s.id } else { &s.title }, title_w))),
                    Span::styled(format!("{kind} {mins} {:>7}", fmt_bytes(s.size)), Style::new().fg(t.muted)),
                    held,
                ]))
            })
            .collect();
        let title = format!("{} · choose sermons", sp.name);
        let note = if whole.is_some() { " (•) already included, because the whole speaker is picked" } else { "" };
        let [list_area, note_area] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(if note.is_empty() { 0 } else { 1 }),
        ])
        .areas(area);
        f.render_stateful_widget(
            List::new(items).block(t.block(&title)).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.srm,
        );
        f.render_widget(Paragraph::new(Line::styled(note, Style::new().fg(t.muted))), note_area);
    }

    fn draw_discover(&mut self, f: &mut Frame, area: Rect) {
        if self.catalog_missing(f, area, "Discover") {
            return;
        }
        let t = self.t;
        let Some(cat) = &self.catalog else { return };
        let mut items = vec![ListItem::new(Line::from(Span::styled(
            format!(" {} Surprise me — add five random speakers to my picks", t.g("★", "*")),
            Style::new().fg(t.gold).add_modifier(Modifier::BOLD),
        )))];
        for &si in &self.disc {
            let sp = &cat.speakers[si];
            let mark = if self.picks.speaker(&sp.name).is_some() {
                format!("  {} picked", t.g("✓", "+"))
            } else {
                String::new()
            };
            items.push(ListItem::new(Line::from(vec![
                Span::raw(format!(" {} ", trunc(&sp.name, 34))),
                Span::styled(
                    format!("{:>6} sermons {:>9}", fmt_n(sp.sermons.len() as u64), fmt_bytes(sp.audio_bytes)),
                    Style::new().fg(t.muted),
                ),
                Span::styled(mark, Style::new().fg(t.green)),
            ])));
        }
        f.render_stateful_widget(
            List::new(items)
                .block(t.block("Discover · ten speakers at random"))
                .highlight_style(t.hl())
                .highlight_symbol(t.g("▸", ">")),
            area,
            &mut self.disc_state,
        );
    }

    fn draw_picks(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let scope = self.scope();
        let wanted = self.picks.wanted_ids(self.catalog.as_ref());
        let held_w = wanted.iter().filter(|id| self.held.contains(*id)).count();
        let bytes_w: u64 = match &self.catalog {
            Some(c) => c.sermons.iter().filter(|s| wanted.contains(&s.id)).map(|s| s.size).sum(),
            None => 0,
        };
        let mut head: Vec<Line> = vec![Line::raw(format!(
            " {} and {}: {}, {}. {} already on this machine.",
            plural(self.picks.speakers.len(), "speaker", "speakers"),
            plural(self.picks.sermons.len(), "single sermon", "single sermons"),
            plural(wanted.len(), "file", "files"),
            fmt_bytes(bytes_w),
            fmt_n(held_w as u64)
        ))];
        let scope_note = match scope.as_str() {
            "picks" => "This node holds your picks and nothing else.",
            "audio" => "This node also holds the whole audio library, so picks matter for video.",
            _ => "This node holds the whole library; picks add nothing new.",
        };
        head.push(Line::styled(format!(" {scope_note}"), Style::new().fg(t.muted)));
        if self.disk.1 > 0 {
            head.push(Line::styled(
                format!(" Room for {} more on this drive ({} free, {} kept for the computer).",
                    fmt_bytes(self.room()), fmt_bytes(self.disk.0), fmt_bytes(self.keep_free())),
                Style::new().fg(if bytes_w > self.room() { t.red } else { t.muted }),
            ));
        }
        if !self.running() && !self.picks.is_empty() {
            head.push(Line::styled(
                " No node is running here — choose Start the node in the menu to download.",
                Style::new().fg(t.gold),
            ));
        }
        let [h, list_area] =
            Layout::vertical([Constraint::Length(head.len() as u16 + 2), Constraint::Min(3)]).areas(area);
        f.render_widget(Paragraph::new(head).wrap(Wrap { trim: true }).block(t.block("My picks")), h);

        if self.picks.speakers.is_empty() {
            let msg = "\n  No speakers picked yet.\n\n  Open Speakers (or Discover), choose a preacher, then\n  \"Download all audio\". They will appear here with their progress.";
            f.render_widget(Paragraph::new(msg).block(t.block("Speakers")), list_area);
            return;
        }
        let idx = self.catalog.as_ref().map(|c| c.speaker_index()).unwrap_or_default();
        let rows: Vec<ListItem> = self
            .picks
            .speakers
            .iter()
            .map(|p| {
                let (held, total) = match (idx.get(&p.name), &self.catalog) {
                    (Some(&si), Some(c)) => {
                        let ids: Vec<&str> = c.speakers[si]
                            .sermons
                            .iter()
                            .map(|&k| &c.sermons[k])
                            .filter(|s| p.video || !s.video)
                            .map(|s| s.id.as_str())
                            .collect();
                        (ids.iter().filter(|id| self.held.contains(**id)).count(), ids.len())
                    }
                    _ => (0, 0),
                };
                let pct = if total > 0 { held as f64 * 100.0 / total as f64 } else { 0.0 };
                let done = held == total && total > 0;
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {} ", trunc(&p.name, 28))),
                    Span::styled(if p.video { "audio+video " } else { "audio       " }, Style::new().fg(t.muted)),
                    Span::styled(bar(&t, pct, 16), Style::new().fg(if done { t.green } else { t.gold })),
                    Span::raw(format!(" {:>6} / {:<6}", fmt_n(held as u64), fmt_n(total as u64))),
                ]))
            })
            .collect();
        f.render_stateful_widget(
            List::new(rows).block(t.block("Speakers")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.pk,
        );
    }

    fn draw_scope(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let cur = self.scope();
        let lock = if self.granted() { "" } else { "   · needs seed node approval" };
        let opts = [
            ("picks", "My picks only".to_string(), "Only the speakers and sermons you choose. Best for a laptop or a small disk."),
            ("audio", format!("The audio library  ({}){lock}", self.lib_label("audio")),
                "Every audio sermon — a seed node. The most useful thing a node can hold."),
            ("full", format!("Everything, audio and video  ({}){lock}", self.lib_label("full")),
                "A complete backup of the library — a seed node with a big drive."),
        ];
        let room = self.room();
        let items: Vec<ListItem> = opts
            .iter()
            .map(|(k, name, desc)| {
                let on = *k == cur.as_str();
                let fit = match self.still_needed(k) {
                    Some((_, b)) if self.disk.1 > 0 => {
                        let ok = b <= room;
                        Line::styled(
                            format!(
                                "      Still to download here: {}.  Room on this drive: {}.{}",
                                fmt_bytes(b),
                                fmt_bytes(room),
                                if ok { "" } else { "  Won't all fit." }
                            ),
                            Style::new().fg(if ok { t.olive } else { t.red }),
                        )
                    }
                    _ => Line::raw(""),
                };
                ListItem::new(Text::from(vec![
                    Line::from(vec![
                        Span::styled(
                            if on { t.g(" (•) ", " (*) ") } else { " ( ) " },
                            Style::new().fg(if on { t.green } else { t.muted }),
                        ),
                        Span::styled(name.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                    ]),
                    Line::styled(format!("      {desc}"), Style::new().fg(t.muted)),
                    fit,
                    Line::raw(""),
                ]))
            })
            .collect();
        f.render_stateful_widget(
            List::new(items).block(t.block("What this node holds")).highlight_style(t.hl()),
            area,
            &mut self.scope_state,
        );
    }

    fn draw_settings(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let [list_area, help_area] = Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(area);
        let lw = 36usize;
        let items: Vec<ListItem> = SETTINGS
            .iter()
            .map(|s| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {:<lw$}", s.label)),
                    Span::styled(show_value(&self.settings, s.key), Style::new().fg(t.gold)),
                ]))
            })
            .collect();
        let i = self.set_state.selected().unwrap_or(0).min(SETTINGS.len() - 1);
        f.render_stateful_widget(
            List::new(items).block(t.block("Settings")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.set_state,
        );
        let how = match SETTINGS[i].kind {
            Kind::Toggle => "Enter switches it on or off.",
            Kind::Preset(_) => "Enter to choose.",
            Kind::Text => "Enter to change.",
        };
        f.render_widget(
            Paragraph::new(format!(" {}  {how}", SETTINGS[i].hint))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(t.muted)),
            help_area,
        );
    }

    /// The Seed Node page: locked (ask for access) until the console approves
    /// this machine, then the controls and progress — as in the desktop app.
    fn draw_seed(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let kv = |k: &str, v: String, c: Color| {
            Line::from(vec![
                Span::styled(format!("{k:<16}"), Style::new().fg(t.muted)),
                Span::styled(v, Style::new().fg(c)),
            ])
        };
        let none = Color::Reset;
        let size = |sc: &str| match self.library_size(sc) {
            Some((n, b)) => format!("{} files · {}", fmt_n(n as u64), fmt_bytes(b)),
            None => self.lib_label(sc),
        };
        let mut lines: Vec<Line> = Vec::new();
        let title;
        if !self.granted() {
            title = "Seed node · needs approval";
            lines.push(Line::raw(
                "A seed node keeps a full copy of the SermonIndex library and shares it around the clock, \
                 so the sermons stay available to everyone — even when our own servers are not.",
            ));
            lines.push(Line::raw(""));
            lines.push(Line::raw(
                "Filling one takes hundreds of gigabytes from our servers, so each machine is approved by a \
                 person on the SermonIndex console first. Picking speakers (Speakers, Discover) needs no approval.",
            ));
            lines.push(Line::raw(""));
            lines.push(kv("This machine", self.node_id.clone(), t.olive));
            let at = self.settings.get("seed_requested_at").and_then(|v| v.as_u64());
            let email = self.settings.get("seed_request_email").and_then(|v| v.as_str()).unwrap_or("");
            let denied = self.seed_word.as_deref() == Some("denied");
            let when_of = |iso: &str| iso.get(..10).unwrap_or(iso).to_string();
            let status = if self.sending {
                ("sending the request…".to_string(), t.gold)
            } else if self.access.is_none() && !self.access_err {
                ("checking with SermonIndex…".to_string(), t.muted)
            } else if denied {
                (
                    format!(
                        "DECLINED{} — this machine's request was not approved",
                        self.declined_at.as_deref().map(|d| format!(" on {}", when_of(d))).unwrap_or_default()
                    ),
                    t.red,
                )
            } else if self.seed_word.as_deref() == Some("none") && at.is_none() {
                ("not requested yet".to_string(), none)
            } else if let Some(at) = at {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let days = now.saturating_sub(at) / 86_400;
                let when = match days {
                    0 => "today".to_string(),
                    1 => "yesterday".to_string(),
                    d => format!("{d} days ago"),
                };
                (format!("requested {when} ({email}) — waiting for approval"), t.gold)
            } else {
                ("not requested yet".to_string(), none)
            };
            lines.push(kv("Status", status.0, status.1));
            if self.access_err {
                lines.push(kv("", "Couldn't reach SermonIndex just now — check the internet connection.".into(), t.red));
            } else if denied {
                lines.push(kv(
                    "",
                    format!(
                        "You can ask again below, or write to {} with the id above.",
                        config::SEED_CONTACT_EMAIL
                    ),
                    t.muted,
                ));
            } else {
                lines.push(kv("", "This page checks by itself every two minutes.".into(), t.muted));
            }
            lines.push(Line::raw(""));
            lines.push(kv("Audio library", size("audio"), none));
            lines.push(kv("Everything", size("full"), none));
            lines.push(kv("This drive", self.disk_words(), none));
        } else {
            title = "Seed node · approved";
            lines.push(Line::styled(
                format!("{} This machine is an approved SermonIndex seed node.", t.g("✓", "+")),
                Style::new().fg(t.green).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::raw(""));
            let scope = self.scope();
            lines.push(kv("Holds", scope_name(&scope).to_string(), none));
            if scope != "picks" {
                if let (Some((n, _)), Some((left, need))) = (self.library_size(&scope), self.still_needed(&scope)) {
                    let have = n.saturating_sub(left);
                    let pct = if n > 0 { have as f64 * 100.0 / n as f64 } else { 0.0 };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<16}", "Progress"), Style::new().fg(t.muted)),
                        Span::styled(bar(&t, pct, 24), Style::new().fg(if left == 0 { t.green } else { t.gold })),
                        Span::raw(format!("  {} of {} ({pct:.1}%)", fmt_n(have as u64), fmt_n(n as u64))),
                    ]));
                    let fits = need <= self.room();
                    lines.push(kv(
                        "Still to fetch",
                        format!(
                            "{} ({}){}",
                            plural(left, "file", "files"),
                            fmt_bytes(need),
                            if fits { "" } else { " — more than fits on this drive" }
                        ),
                        if fits { none } else { t.red },
                    ));
                }
            } else {
                lines.push(kv("", "Choose the audio library or everything below.".into(), t.gold));
            }
            lines.push(kv("This drive", self.disk_words(), none));
            match &self.stats {
                Some(st) => {
                    let n = &st["node"];
                    let u = |k: &str| n[k].as_u64().unwrap_or(0);
                    lines.push(kv("Peers", format!("{} connected", u("peers")), if u("peers") > 0 { t.green } else { none }));
                    lines.push(kv("Shared so far", fmt_bytes(u("uploaded_bytes")), none));
                    lines.push(kv("Reachable", n["reachable"].as_str().unwrap_or("unknown").to_string(), none));
                }
                None => lines.push(kv("Node", "not running — choose Start the node".into(), t.gold)),
            }
        }
        let inner_w = area.width.saturating_sub(4) as usize;
        let rows: usize = lines
            .iter()
            .map(|l| wrap_count(&l.spans.iter().map(|s| s.content.as_ref()).collect::<String>(), inner_w))
            .sum();
        let n_act = self.seed_actions().len() as u16;
        let h = (rows as u16 + 2).min(area.height.saturating_sub(n_act + 2).max(3));
        let [info, list_area] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(area);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(t.block(title).padding(ratatui::widgets::Padding::horizontal(1))),
            info,
        );
        let items: Vec<ListItem> =
            self.seed_actions().into_iter().map(|(_, l)| ListItem::new(format!(" {l}"))).collect();
        f.render_stateful_widget(
            List::new(items).block(t.block("")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.seed_state,
        );
    }

    fn draw_email(&self, f: &mut Frame, area: Rect, v: &str, err: Option<&str>) {
        let t = self.t;
        let w = 70u16.min(area.width);
        let help = "Type an email address we can reply to, then press Enter.\n\n\
                    A person at SermonIndex reviews each request and turns seed node access on for this \
                    machine from the console. The node notices by itself and starts — nothing to reinstall. \
                    The email is used only to reply about this request.";
        let inner = w.saturating_sub(4) as usize;
        let rows: usize = help.split('\n').map(|l| wrap_count(l, inner)).sum();
        let mut lines: Vec<Line> = help.split('\n').map(|l| Line::styled(l.to_string(), Style::new().fg(t.cream))).collect();
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!(" {}{} ", v, t.g("▏", "_")),
            Style::new().fg(t.cream).bg(t.olive_dk).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        match err {
            Some(e) => lines.push(Line::styled(e.to_string(), Style::new().fg(t.red))),
            None => lines.push(Line::styled("Enter to send  ·  Esc to cancel", Style::new().fg(t.muted))),
        }
        let r = centered(area, w, (rows as u16 + 6).min(area.height));
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(t.block("Request seed node access").padding(ratatui::widgets::Padding::horizontal(1))),
            r,
        );
    }

    fn draw_dashboard(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let url = |u: String| Span::styled(u, Style::new().fg(t.gold).add_modifier(Modifier::BOLD));
        let label = |s: &str| Span::styled(format!(" {s:<26}"), Style::new().fg(t.muted));
        let mut lines = vec![
            Line::raw(" The node serves a live stats page — the same display as the kiosk screen —"),
            Line::raw(" to any web browser. Type one of these addresses into the browser's address bar:"),
            Line::raw(""),
            Line::from(vec![label("On this computer"), url(dash_url("localhost"))]),
            Line::from(vec![label("Full-screen display"), url(format!("{}?view=compact", dash_url("localhost")))]),
        ];
        match &self.lan {
            Some(ip) => {
                lines.push(Line::from(vec![label("On a phone, tablet or TV"), url(dash_url(ip))]));
                lines.push(Line::styled(
                    "                            (on the same Wi-Fi or network as this computer)",
                    Style::new().fg(t.muted),
                ));
            }
            None => lines.push(Line::styled(
                " (No local network address found, so it can only be opened on this computer.)",
                Style::new().fg(t.muted),
            )),
        }
        lines.push(Line::raw(""));
        if self.running() {
            lines.push(Line::styled(" The node is running, so these work now.", Style::new().fg(t.green)));
        } else {
            lines.push(Line::styled(
                " The dashboard only answers while the node runs — choose Start the node first.",
                Style::new().fg(t.gold),
            ));
        }
        let h = (lines.len() as u16 + 2).min(area.height);
        let [info, list_area] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(area);
        f.render_widget(Paragraph::new(lines).block(t.block("Dashboard in a browser")), info);
        let items: Vec<ListItem> =
            self.dash_actions().into_iter().map(|(_, l)| ListItem::new(format!(" {l}"))).collect();
        f.render_stateful_widget(
            List::new(items).block(t.block("")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.dash_state,
        );
    }

    fn draw_updates(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let [info, list_area] = Layout::vertical([Constraint::Length(7), Constraint::Min(3)]).areas(area);
        let state = match &self.latest {
            None => "Checking for a newer release…".to_string(),
            Some(None) => "This is the newest release.".to_string(),
            Some(Some(v)) => format!("v{v} is available."),
        };
        let lines = vec![
            Line::raw(format!(" Running v{}.  {state}", update::current())),
            Line::raw(""),
            Line::styled(
                " Installing checks the download, replaces the program and restarts the node.",
                Style::new().fg(t.muted),
            ),
            Line::styled(" Your library and settings stay as they are. It may ask for your password.", Style::new().fg(t.muted)),
            Line::styled(" Same as typing:  sermonindex-node upgrade", Style::new().fg(t.muted)),
        ];
        f.render_widget(Paragraph::new(lines).block(t.block("Updates")), info);
        let items: Vec<ListItem> =
            self.upd_actions().into_iter().map(|(_, l)| ListItem::new(format!(" {l}"))).collect();
        f.render_stateful_widget(
            List::new(items).block(t.block("")).highlight_style(t.hl()).highlight_symbol(t.g("▸", ">")),
            list_area,
            &mut self.upd_state,
        );
    }

    fn draw_help(&mut self, f: &mut Frame, area: Rect) {
        let t = self.t;
        let k = |a: &str, b: &str| {
            Line::from(vec![Span::styled(format!("  {a:<14}"), Style::new().fg(t.gold)), Span::raw(b.to_string())])
        };
        let c = |a: &str, b: &str| {
            Line::from(vec![
                Span::styled(format!("  {a:<38}"), Style::new().fg(t.olive)),
                Span::styled(b.to_string(), Style::new().fg(t.muted)),
            ])
        };
        let lines = vec![
            Line::styled(" Keys", Style::new().add_modifier(Modifier::BOLD)),
            k("↑ ↓", "move"),
            k("Enter  →", "open / choose"),
            k("Esc  ←", "back to the menu"),
            k("Space", "pick or unpick (Speakers, sermon lists)"),
            k("PgUp PgDn", "a page at a time"),
            k("q  Ctrl-C", "quit"),
            Line::raw(""),
            Line::styled(" The same things as commands, for scripts", Style::new().add_modifier(Modifier::BOLD)),
            c("sermonindex-node status", "what the node is doing"),
            c("sermonindex-node config", "every setting"),
            c("sermonindex-node config scope picks", "hold only your picks"),
            c("sermonindex-node refresh", "fetch anything new now"),
            c("sermonindex-node config keep-free 20GB", "always leave 20 GB empty"),
            c("sermonindex-node upgrade", "install the newest release"),
            c("sermonindex-node help", "the full list"),
        ];
        f.render_widget(Paragraph::new(lines).block(t.block("Help")), area);
    }

    fn draw_edit(&self, f: &mut Frame, area: Rect, e: &Edit) {
        let t = self.t;
        let s = &SETTINGS[e.idx];
        let w = 70u16.min(area.width);
        let inner = w.saturating_sub(4) as usize;
        let help = if s.custom.is_empty() { s.hint } else { s.custom };
        let mut lines: Vec<Line> = help.split('\n').map(|l| Line::styled(l.to_string(), Style::new().fg(t.cream))).collect();
        let help_rows: usize = help.split('\n').map(|l| wrap_count(l, inner)).sum();
        lines.push(Line::raw(""));
        let unit = match (s.unit, s.key) {
            (Unit::Mbps, _) => "  Mbps",
            (_, "monthly-cap" | "keep-free") => "  GB",
            _ => "",
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}{} ", e.value, t.g("▏", "_")),
                Style::new().fg(t.cream).bg(t.olive_dk).add_modifier(Modifier::BOLD),
            ),
            Span::styled(unit, Style::new().fg(t.muted)),
        ]));
        lines.push(Line::raw(""));
        match &e.error {
            Some(err) => lines.push(Line::styled(format!("Not saved: {err}"), Style::new().fg(t.red))),
            None => lines.push(Line::styled("Enter to save  ·  Esc to cancel", Style::new().fg(t.muted))),
        }
        let h = (help_rows as u16 + 6).min(area.height);
        let r = centered(area, w, h);
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(t.block(s.label).padding(ratatui::widgets::Padding::horizontal(1))),
            r,
        );
    }

    fn draw_choose(&self, f: &mut Frame, area: Rect, c: &mut Choose) {
        let t = self.t;
        let w = 72u16.min(area.width);
        let inner = w.saturating_sub(4) as usize;
        let text_rows: usize = c.lines.iter().map(|l| wrap_count(l, inner)).sum();
        let h = (text_rows as u16 + c.opts.len() as u16 + 5).min(area.height);
        let r = centered(area, w, h);
        f.render_widget(Clear, r);
        let block = t.block(&c.title).padding(ratatui::widgets::Padding::horizontal(1));
        let inner_r = block.inner(r);
        f.render_widget(block, r);
        let [top, list_area, keys] = Layout::vertical([
            Constraint::Length(text_rows as u16 + 1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner_r);
        let lines: Vec<Line> = c.lines.iter().map(|l| Line::raw(l.clone())).collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), top);
        let items: Vec<ListItem> = c
            .opts
            .iter()
            .enumerate()
            .map(|(k, (l, _))| {
                let mark = match c.current {
                    Some(at) if at == k => t.g("(•) ", "(*) "),
                    Some(_) => "    ",
                    None => "",
                };
                ListItem::new(Line::from(vec![
                    Span::styled(mark, Style::new().fg(t.green)),
                    Span::raw(l.clone()),
                ]))
            })
            .collect();
        f.render_stateful_widget(
            List::new(items).highlight_style(t.hl()).highlight_symbol(t.g("▸ ", "> ")),
            list_area,
            &mut c.state,
        );
        f.render_widget(
            Paragraph::new(Line::styled("↑↓ choose  ·  Enter  ·  Esc cancel", Style::new().fg(t.muted))),
            keys,
        );
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect, c: &Confirm) {
        let t = self.t;
        let h = (c.lines.len() as u16 + 5).min(area.height);
        let r = centered(area, 68, h);
        f.render_widget(Clear, r);
        let mut lines: Vec<Line> = c.lines.iter().map(|l| Line::raw(format!(" {l}"))).collect();
        lines.push(Line::raw(""));
        lines.push(Line::styled(" y / Enter  yes      n / Esc  no", Style::new().fg(t.gold)));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(t.block(&c.title)), r);
    }
}

/// `/home/greg/.sermonindex/downloads` → `~/.sermonindex/downloads`.
fn tidy_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn scope_name(s: &str) -> &'static str {
    match s {
        "picks" => "your picks only",
        "full" => "the whole library, audio and video",
        _ => "the audio library",
    }
}

fn toggle_value(s: &Value, key: &str) -> bool {
    match key {
        "dht" => config::dht_enabled(s),
        "natpmp" => config::natpmp_enabled(s),
        "p2p" => config::p2p_enabled(s),
        _ => false,
    }
}

/// What to pre-fill when editing — the form `config` accepts.
fn raw_value(s: &Value, key: &str) -> String {
    let b = |k: &str| s.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let st = |k: &str| s.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match key {
        "upload" => {
            if b("upload_limit_enabled") {
                s.get("upload_limit_kbps").and_then(|v| v.as_u64()).unwrap_or(0).to_string()
            } else {
                "off".into()
            }
        }
        "schedule" => {
            if b("seed_schedule_enabled") {
                format!("{}-{}", st("seed_start"), st("seed_end"))
            } else {
                "off".into()
            }
        }
        "monthly-cap" => {
            if b("monthly_cap_enabled") {
                format!("{}GB", s.get("monthly_cap_gb").and_then(|v| v.as_f64()).unwrap_or(0.0))
            } else {
                "off".into()
            }
        }
        "dir" => config::downloads_dir(s).display().to_string(),
        "downloads" => config::download_workers(s).to_string(),
        "source" => {
            let v = st("content_mode");
            if v.is_empty() {
                "auto".into()
            } else {
                v
            }
        }
        "window" => match s.get("active_torrents").and_then(|v| v.as_u64()) {
            Some(n) => n.to_string(),
            None => "auto".into(),
        },
        "keep-free" => {
            let gb = config::keep_free_bytes(s) as f64 / 1024.0 / 1024.0 / 1024.0;
            format!("{}", (gb * 10.0).round() / 10.0)
        }
        _ => String::new(),
    }
}

/// Is the current setting (`cur`, from raw_value) the preset `preset`?
fn same_value(key: &str, preset: &str, cur: &str) -> bool {
    let num = |v: &str| v.trim().trim_end_matches("GB").trim_end_matches("gb").parse::<f64>().ok();
    match key {
        "monthly-cap" | "keep-free" | "upload" | "downloads" | "window" => match (num(preset), num(cur)) {
            (Some(a), Some(b)) => (a - b).abs() < 0.01,
            _ => preset == cur,
        },
        _ => preset == cur,
    }
}

/// What was typed into a custom box → what `config` takes.
fn to_config(unit: Unit, typed: &str) -> std::result::Result<String, String> {
    let v = typed.trim();
    if v.is_empty() {
        return Err("type a value first (or Esc to cancel)".into());
    }
    match unit {
        Unit::Plain => Ok(v.to_string()),
        Unit::Mbps => {
            if v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("unlimited") {
                return Ok("off".into());
            }
            let t = v.to_lowercase();
            let t = t.trim_end_matches("mbps").trim_end_matches("mbit/s").trim();
            let n: f64 = t.parse().map_err(|_| format!("“{v}” is not a number — type just the number, e.g. 3"))?;
            if n < 0.0 {
                return Err("the speed can't be negative".into());
            }
            if n == 0.0 {
                return Ok("off".into());
            }
            // 1 Mbps = 125 KB/s.
            Ok(((n * 125.0).round() as u64).max(1).to_string())
        }
    }
}

/// Rows `s` takes when word-wrapped to `w` columns (at least one).
fn wrap_count(s: &str, w: usize) -> usize {
    if w == 0 {
        return 1;
    }
    let mut rows = 1;
    let mut col = 0;
    for word in s.split(' ') {
        let n = word.chars().count();
        if col == 0 {
            col = n;
        } else if col + 1 + n <= w {
            col += 1 + n;
        } else {
            rows += 1;
            col = n;
        }
        while col > w {
            rows += 1;
            col -= w;
        }
    }
    rows
}

/// What to show in the settings list — in words, not config syntax.
fn show_value(s: &Value, key: &str) -> String {
    let raw = raw_value(s, key);
    match key {
        "dht" | "natpmp" | "p2p" => {
            if toggle_value(s, key) {
                "On".into()
            } else {
                "Off".into()
            }
        }
        "upload" => {
            if raw == "off" {
                "Unlimited".into()
            } else {
                let kb: f64 = raw.parse().unwrap_or(0.0);
                let mbps = kb / 125.0;
                if (mbps - mbps.round()).abs() < 0.05 {
                    format!("{} Mbps", mbps.round())
                } else {
                    format!("{mbps:.1} Mbps")
                }
            }
        }
        "keep-free" => format!("{raw} GB"),
        "window" => {
            if raw == "auto" {
                format!("Automatic ({})", fmt_n(config::active_torrents(s) as u64))
            } else {
                fmt_n(raw.parse().unwrap_or(0))
            }
        }
        "schedule" => {
            if raw == "off" {
                "Around the clock".into()
            } else {
                raw.replace('-', " – ")
            }
        }
        "monthly-cap" => {
            if raw == "off" {
                "No limit".into()
            } else {
                let gb: f64 = raw.trim_end_matches("GB").parse().unwrap_or(0.0);
                if gb >= 1024.0 && (gb / 1024.0).fract().abs() < 0.01 {
                    format!("{} TB a month", gb / 1024.0)
                } else {
                    format!("{gb} GB a month")
                }
            }
        }
        "source" => match raw.as_str() {
            "archive" => "Archive.org first".into(),
            "cdn" => "SermonIndex CDN first".into(),
            _ => "Automatic".into(),
        },
        "dir" => tidy_path(&config::downloads_dir(s)),
        _ => raw,
    }
}

/// 0.03% stays 0.03%, 42.5% is 42.5%.
fn fmt_pct(p: f64) -> String {
    if p > 0.0 && p < 1.0 {
        format!("{p:.2}%")
    } else {
        format!("{p:.1}%")
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", fmt_n(n as u64), if n == 1 { one } else { many })
}

// ── entry point ──────────────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        bail!("the menu needs an interactive terminal — run `sermonindex-node help` for the commands");
    }
    let (tx, rx) = mpsc::channel();
    spawn_workers(tx.clone());
    let mut app = App::new(tx.clone());
    spawn_access_watch(tx, app.node_id.clone());

    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    // Whatever happens, give the terminal back the way we found it.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
        prev(info);
    }));

    let res = event_loop(&mut term, &mut app, &rx);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res?;

    match app.after {
        After::Nothing => {
            if app.running() {
                println!("The node keeps running. `sermonindex-node` opens the menu again.");
            }
            Ok(())
        }
        After::Upgrade => crate::run_upgrade(),
    }
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &Receiver<Msg>,
) -> Result<()> {
    loop {
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
        }
        app.log.poll();
        if let Some((what, cmd)) = app.run_cmd.take() {
            let ok = run_in_terminal(term, &what, &cmd)?;
            app.say(if ok {
                format!("{what} — done.")
            } else {
                format!("{what} did not work — see the message it printed.")
            });
        }
        term.draw(|f| app.draw(f))?;
        if app.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.on_key(k);
                }
            }
        }
    }
}

/// Step out of the menu to run a command that needs the real terminal (sudo
/// asking for a password), then come straight back.
fn run_in_terminal(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    what: &str,
    cmd: &[String],
) -> Result<bool> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, ratatui::crossterm::cursor::Show)?;
    println!("\n{what}:  {}\n", cmd.join(" "));
    if cmd.first().map(|c| c == "sudo").unwrap_or(false) {
        println!("If it asks, type YOUR password and press Enter (nothing shows while you type).\n");
    }
    let ok = match Command::new(&cmd[0]).args(&cmd[1..]).status() {
        Ok(st) => st.success(),
        Err(e) => {
            println!("Could not run {}: {e}", cmd[0]);
            false
        }
    };
    if !ok {
        println!("\nPress Enter to go back to the menu.");
        let mut s = String::new();
        let _ = io::stdin().read_line(&mut s);
    }
    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen)?;
    term.clear()?;
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    pub(super) fn snapshot(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app_with_catalog() -> App {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(tx);
        let raw = br#"{"s":[["Leonard Ravenhill"],["Zac Poonen"],["A. W. Tozer"]],
            "c":[["a1","Why Revival Tarries",0,0,0,3000,40000,"","",0,0],
                 ["a2","Weeping Between the Porch and the Altar",0,0,0,2600,35000,"","",0,0],
                 ["z1","The Cross and the Self Life",1,0,0,2400,30000,"","",0,0],
                 ["z2","Overcoming",1,0,0,2000,300000,"","",1,0],
                 ["t1","The Pursuit of God",2,0,0,1800,25000,"","",0,0]]}"#;
        app.catalog = Some(Catalog::parse(raw).unwrap());
        app.rebuild_view();
        app.shuffle();
        app
    }

    /// Every screen draws, at sizes from a phone-width SSH window to a big
    /// monitor, without panicking (ratatui panics on out-of-bounds areas).
    #[test]
    fn screens_render_at_common_sizes() {
        let mut app = app_with_catalog();
        for (w, h) in [(80, 24), (120, 40), (60, 20), (40, 12), (24, 8)] {
            for s in [
                Screen::Home, Screen::Status, Screen::Speakers, Screen::Speaker(0), Screen::Sermons(1),
                Screen::Discover, Screen::MyPicks, Screen::Scope, Screen::Settings, Screen::Dashboard,
                Screen::Seed, Screen::Connections, Screen::Updates, Screen::Help,
            ] {
                app.stack = vec![Screen::Home, s];
                let _ = snapshot(&mut app, w, h);
            }
        }
    }

    #[test]
    fn overlays_render_at_common_sizes() {
        let mut app = app_with_catalog();
        for (w, h) in [(120, 40), (80, 24), (40, 12), (24, 8)] {
            for i in 0..SETTINGS.len() {
                app.stack = vec![Screen::Home, Screen::Settings];
                app.overlay = Overlay::None;
                if matches!(SETTINGS[i].kind, Kind::Preset(_)) {
                    app.preset_box(i);
                    let _ = snapshot(&mut app, w, h);
                }
                app.overlay = Overlay::Edit(Edit { idx: i, value: "12".into(), error: Some("nope".into()) });
                let _ = snapshot(&mut app, w, h);
            }
            app.overlay = Overlay::None;
            app.stack = vec![Screen::Home];
            app.start_prompt();
            let _ = snapshot(&mut app, w, h);
        }
    }

    #[test]
    fn side_menu_and_live_panel_show() {
        let mut app = app_with_catalog();
        app.log.lines.push_back("[download] 3 files to fetch".into());
        app.disk = (230 << 30, 1000 << 30);
        let shot = snapshot(&mut app, 120, 40);
        assert!(shot.contains("Start the node"), "{shot}");
        assert!(shot.contains("Node activity"), "{shot}");
        assert!(shot.contains("[download] 3 files to fetch"), "{shot}");
        assert!(shot.contains("230.0 GB free"), "{shot}");
        // Opening a section keeps the menu on screen.
        app.stack = vec![Screen::Home, Screen::Settings];
        let shot = snapshot(&mut app, 120, 40);
        assert!(shot.contains("Menu") && shot.contains("Upload speed limit"), "{shot}");
    }

    #[test]
    fn library_is_locked_until_approved() {
        let mut app = app_with_catalog();
        app.access = Some(false);
        app.stack = vec![Screen::Home, Screen::Seed];
        let shot = snapshot(&mut app, 120, 40);
        assert!(shot.contains("needs approval") && shot.contains("Request seed node access"), "{shot}");
        // Choosing the audio library on the scope screen sends you to ask.
        app.stack = vec![Screen::Home, Screen::Scope];
        app.scope_state.select(Some(1));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.screen(), Screen::Seed);
        // Declined on the console: said plainly, with a way to ask again.
        app.seed_word = Some("denied".into());
        app.declined_at = Some("2026-10-02T10:00:00Z".into());
        app.stack = vec![Screen::Home, Screen::Seed];
        let shot = snapshot(&mut app, 120, 40);
        assert!(shot.contains("DECLINED on 2026-10-02") && shot.contains("Ask again"), "{shot}");
        assert!(shot.contains("Seed node  (declined)"), "{shot}");
        // With approval the page shows the controls instead.
        app.access = Some(true);
        let shot = snapshot(&mut app, 120, 40);
        assert!(shot.contains("approved") && shot.contains("Hold the audio library"), "{shot}");
        for (w, h) in [(80, 24), (40, 12), (24, 8)] {
            app.access = Some(false);
            let _ = snapshot(&mut app, w, h);
            app.overlay = Overlay::Email("a@b".into(), Some("bad".into()));
            let _ = snapshot(&mut app, w, h);
            app.overlay = Overlay::None;
        }
    }

    #[test]
    fn custom_values_convert_for_config() {
        assert_eq!(to_config(Unit::Mbps, "3").unwrap(), "375");
        assert_eq!(to_config(Unit::Mbps, "0").unwrap(), "off");
        assert_eq!(to_config(Unit::Mbps, "2.5 mbps").unwrap(), "313");
        assert!(to_config(Unit::Mbps, "fast").is_err());
        assert_eq!(to_config(Unit::Plain, " 750 ").unwrap(), "750");
        assert!(same_value("monthly-cap", "1024", "1024GB"));
        assert!(same_value("keep-free", "10", "10"));
        assert!(!same_value("upload", "625", "off"));
        assert_eq!(wrap_count("aaa bbb ccc", 7), 2);
        assert_eq!(wrap_count("", 10), 1);
    }

    #[test]
    fn log_tail_follows_growth_and_rotation() {
        let dir = std::env::temp_dir().join(format!("si-logtail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("node.log");
        std::fs::write(&p, "one\n\x1b[32mtwo\x1b[0m\npart").unwrap();
        let mut lt = LogTail::new();
        lt.poll_path(&p);
        assert_eq!(lt.lines.iter().cloned().collect::<Vec<_>>(), vec!["one", "two"]);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"ial\nthree\n").unwrap();
        lt.poll_path(&p);
        assert_eq!(lt.lines.back().unwrap(), "three");
        assert!(lt.lines.contains(&"partial".to_string()));
        std::fs::write(&p, "fresh\n").unwrap(); // rotated
        lt.poll_path(&p);
        assert_eq!(lt.lines.back().unwrap(), "fresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_filters_speakers() {
        let mut app = app_with_catalog();
        app.stack = vec![Screen::Home, Screen::Speakers];
        for c in "rav".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.view.len(), 1);
        let names = &app.catalog.as_ref().unwrap().speakers;
        assert_eq!(names[app.view[0]].name, "Leonard Ravenhill");
    }

    #[test]
    fn wanted_ids_respects_audio_only_picks() {
        let app = app_with_catalog();
        let mut p = Picks::default();
        p.set_speaker("Zac Poonen", false);
        let w = p.wanted_ids(app.catalog.as_ref());
        assert!(w.contains("z1") && !w.contains("z2"), "audio pick must not include video");
        p.set_speaker("Zac Poonen", true);
        assert!(p.wanted_ids(app.catalog.as_ref()).contains("z2"));
    }
}
