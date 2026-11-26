//! Finding `contextstream-mcp watch` processes (the sync bridge), for the
//! tests that keep CLI tests from leaving one running. It reads `/proc`, so
//! it is Linux only, and sees only this user's processes: another user's
//! environment cannot be read.

#![allow(dead_code)] // Each test target uses its own part.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A `contextstream-mcp watch` process of this user.
#[derive(Debug)]
pub struct Watcher {
    pub pid: u32,
    /// Its `HOME`.
    pub home: Option<PathBuf>,
    /// When it started, in clock ticks since boot.
    pub started: u64,
}

/// Every `contextstream-mcp watch` process this user runs.
pub fn watchers() -> Vec<Watcher> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(watcher)
        .collect()
}

/// The watchers whose `HOME` is `home`.
pub fn watchers_for(home: &Path) -> Vec<Watcher> {
    watchers()
        .into_iter()
        .filter(|watcher| watcher.home.as_deref() == Some(home))
        .collect()
}

/// Wait up to `limit` for a watcher with `HOME` `home` to show up.
pub fn wait_for_watcher(home: &Path, limit: Duration) -> Vec<Watcher> {
    let deadline = Instant::now() + limit;
    loop {
        let found = watchers_for(home);
        if !found.is_empty() || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn watcher(pid: u32) -> Option<Watcher> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut args = cmdline.split(|byte| *byte == 0);
    let program = args.next()?;
    let name = program.rsplit(|byte| *byte == b'/').next()?;
    if name != b"contextstream-mcp" || args.next()? != b"watch" {
        return None;
    }
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let home = environ
        .split(|byte| *byte == 0)
        .find_map(|pair| pair.strip_prefix(b"HOME="))
        .map(|home| PathBuf::from(String::from_utf8_lossy(home).into_owned()));
    Some(Watcher {
        pid,
        home,
        started: started(pid)?,
    })
}

/// Field `index` of `/proc/<pid>/stat`, counted after the command name: 0 is
/// the state, 1 the parent, 19 the start time.
fn stat_field(pid: u32, index: usize) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .nth(index)
        .map(String::from)
}

/// The process that started `pid`.
pub fn parent(pid: u32) -> Option<u32> {
    stat_field(pid, 1)?.parse().ok()
}

/// When `pid` started, in clock ticks since boot.
pub fn started(pid: u32) -> Option<u64> {
    stat_field(pid, 19)?.parse().ok()
}

fn running(pid: u32) -> bool {
    stat_field(pid, 0).is_some_and(|state| state != "Z" && state != "X")
}

/// Whether `pid` runs `binary`, byte for byte. A watcher's executable is
/// often a copy in a home that has since been removed, which `/proc` still
/// reads.
pub fn runs(pid: u32, binary: &Path) -> bool {
    let exe = PathBuf::from(format!("/proc/{pid}/exe"));
    match (std::fs::metadata(&exe), std::fs::metadata(binary)) {
        (Ok(running), Ok(built)) if running.len() == built.len() => same_bytes(&exe, binary),
        _ => false,
    }
}

fn same_bytes(left: &Path, right: &Path) -> bool {
    let (Ok(mut left), Ok(mut right)) = (std::fs::File::open(left), std::fs::File::open(right))
    else {
        return false;
    };
    let (mut a, mut b) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        match (fill(&mut left, &mut a), fill(&mut right, &mut b)) {
            (Ok(0), Ok(0)) => return true,
            (Ok(n), Ok(m)) if n == m && a[..n] == b[..m] => {}
            _ => return false,
        }
    }
}

fn fill(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Stop `pid` and wait until it is gone: TERM, then KILL after five seconds.
pub fn stop(pid: u32) {
    for (signal, wait) in [("-TERM", 5), ("-KILL", 5)] {
        let _ = std::process::Command::new("kill")
            .args([signal, &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(wait);
        while running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !running(pid) {
            return;
        }
    }
}
