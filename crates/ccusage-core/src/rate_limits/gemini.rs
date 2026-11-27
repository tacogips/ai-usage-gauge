use super::{date, window};
use crate::types::{RateLimitErrorCode, RateLimitObservation};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn discover() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter()
        .map(|p| p.join("agy"))
        .find(|p| executable(p))
}
fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
pub fn parse(v: &Value, now: DateTime<Utc>) -> Option<RateLimitObservation> {
    if v["product"] != "antigravity" {
        return None;
    }
    let mut windows = Vec::new();
    for (key, seconds) in [("gemini-5h", 18_000), ("gemini-weekly", 604_800)] {
        let q = &v["quota"][key];
        if q["disabled"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(remaining) = q["remaining_fraction"]
            .as_f64()
            .filter(|n| (0.0..=1.0).contains(n))
        else {
            continue;
        };
        let reset = date(q["reset_time"].as_str()).or_else(|| {
            q["reset_in_seconds"]
                .as_f64()
                .filter(|s| s.is_finite() && *s > 0.0 && *s < i64::MAX as f64 / 1000.0)
                .and_then(|s| {
                    now.checked_add_signed(chrono::Duration::try_milliseconds((s * 1000.0) as i64)?)
                })
        });
        if let Ok(w) = window(seconds, (1.0 - remaining) * 100.0, reset) {
            windows.push(w);
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(RateLimitObservation {
        plan_type: v["plan_tier"].as_str().map(str::to_owned),
        observed_at: now,
        windows,
        credits: None,
    })
}
pub fn fetch(binary: &Path, timeout: Duration) -> Result<RateLimitObservation, RateLimitErrorCode> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let temp = builder
        .tempdir()
        .map_err(|_| RateLimitErrorCode::InvalidResponse)?;
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 36,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|_| RateLimitErrorCode::InvalidResponse)?;
    #[cfg(unix)]
    if let Some(fd) = pair.master.as_raw_fd() {
        unsafe {
            libc::fcntl(
                fd,
                libc::F_SETFL,
                libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
            );
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    let mut cmd = portable_pty::CommandBuilder::new(binary);
    cmd.arg("--settings");
    cmd.arg(r#"{"statusLine":{"type":"command","command":"umask 077; /bin/cat > status.tmp && /bin/mv -f status.tmp status.json"}}"#);
    cmd.cwd(temp.path());
    cmd.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|_| RateLimitErrorCode::CliMissing)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|_| {
        let _ = child.kill();
        let _ = child.wait();
        RateLimitErrorCode::InvalidResponse
    })?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drain_stop = stop.clone();
    let drain = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        while !drain_stop.load(std::sync::atomic::Ordering::Relaxed) {
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    });
    let mut writer = pair.master.take_writer().ok();
    let started = Instant::now();
    let result = loop {
        if let Ok(bytes) = std::fs::read(temp.path().join("status.json")) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(obs) = parse(&value, Utc::now()) {
                    break Ok(obs);
                }
            }
        }
        if child.try_wait().ok().flatten().is_some() {
            break Err(RateLimitErrorCode::InvalidResponse);
        }
        if started.elapsed() >= timeout {
            break Err(RateLimitErrorCode::Timeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if let Some(writer) = writer.as_mut() {
        let _ = writer.write_all(&[4]);
        let _ = writer.flush();
    }
    let end = Instant::now() + Duration::from_millis(250);
    while Instant::now() < end && child.try_wait().ok().flatten().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().ok().flatten().is_none() {
        #[cfg(unix)]
        if let Some(pid) = child.process_id() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        let _ = child.kill();
    }
    let _ = child.wait();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(writer);
    drop(pair.master);
    let _ = drain.join();
    result
}
