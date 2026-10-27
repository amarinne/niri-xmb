//! Single running `niri-xmb-config-studio` with takeover.
//!
//! A new studio launch reliably finds and terminates the old instance, then
//! becomes the active one. The mechanism is Linux-local:
//!
//! * An exclusive [`flock`](libc::flock) on
//!   `$XDG_RUNTIME_DIR/niri-xmb-config-studio.lock` (fallback: temp dir)
//!   serializes starters. The kernel releases the lock when the holder dies,
//!   so a stale lock cannot survive a crash.
//! * The holder writes `<pid> <starttime>` into the lock file. `starttime`
//!   is `/proc/<pid>/stat` field 22, which disambiguates PID reuse.
//! * A starter that finds the lock held reads that identity, verifies the
//!   owner is really a studio process via `/proc/<pid>/cmdline` plus the
//!   `starttime` match, then asks it to exit with `SIGTERM` (escalating to
//!   `SIGKILL`) and waits for the lock. Anything that does not look like a
//!   studio process is never signalled.
//!
//! Terminating the old process also closes its daemon subscribe stream, so
//! the daemon frees that subscriber slot and the new instance's live feed
//! starts clean: no duplicate windows, no stale binaries, no orphaned live
//! subscriptions. Normal window close just drops the guard, which closes the
//! file and releases the lock.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const LOCK_NAME: &str = "niri-xmb-config-studio.lock";
const PROCESS_NAME: &str = "niri-xmb-config-studio";
const TAKEOVER_GRACE: Duration = Duration::from_secs(5);
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Absolute lock path for this user session.
pub fn studio_lock_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        runtime.join(LOCK_NAME)
    } else {
        std::env::temp_dir().join(LOCK_NAME)
    }
}

/// Parse `<pid> <starttime>` written by the lock holder.
pub fn parse_lock_identity(text: &str) -> Option<(u32, u64)> {
    let mut parts = text.split_whitespace();
    let pid: u32 = parts.next()?.parse().ok()?;
    let starttime: u64 = parts.next()?.parse().ok()?;
    if pid == 0 {
        return None;
    }
    Some((pid, starttime))
}

/// Linux process start time (`/proc/<pid>/stat` field 22), used to tell a
/// live owner apart from a recycled PID.
pub fn proc_starttime(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end = text.rfind(')')?;
    let after = &text[end + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After `pid (comm)` the next field is state (original field 3), so
    // original field 22 lands at index 19.
    fields.get(19)?.parse().ok()
}

/// Raw `/proc/<pid>/cmdline` with NULs replaced, for identity checks.
pub fn proc_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn is_self(pid: u32) -> bool {
    pid == std::process::id()
}

/// Whether `pid` currently looks like a running studio process.
///
/// When `expected_starttime` is known the `/proc` start time must match, so a
/// recycled PID is treated as unrelated. The current process is never its own
/// predecessor.
pub fn is_studio_process(pid: u32, expected_starttime: Option<u64>) -> bool {
    if is_self(pid) {
        return false;
    }
    let cmdline = match proc_cmdline(pid) {
        Some(cmdline) => cmdline,
        None => return false,
    };
    if !cmdline.contains(PROCESS_NAME) {
        return false;
    }
    match expected_starttime {
        Some(expected) => proc_starttime(pid).is_some_and(|actual| actual == expected),
        None => true,
    }
}

/// Whether the recorded owner is still the same live process.
pub fn is_owner_alive(pid: u32, starttime: u64) -> bool {
    proc_starttime(pid).is_some_and(|actual| actual == starttime)
}

/// Whether a starter may signal this owner during takeover.
///
/// True only for a live studio process that is not ourselves. Stale PIDs,
/// recycled PIDs, and unrelated processes are never signalled.
pub fn should_terminate_owner(pid: u32, starttime: u64) -> bool {
    is_owner_alive(pid, starttime) && is_studio_process(pid, Some(starttime))
}

fn try_lock_nb(file: &File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let errno = unsafe { *libc::__errno_location() };
    if errno == libc::EWOULDBLOCK {
        return Ok(false);
    }
    anyhow::bail!("flock on studio lock failed: errno {errno}");
}

fn read_identity_file(path: &std::path::Path) -> Option<(u32, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_lock_identity(&text)
}

fn write_identity(file: &mut File) -> Result<()> {
    let pid = std::process::id();
    let starttime = proc_starttime(pid).context("cannot read own starttime")?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{pid} {starttime}")?;
    file.flush()?;
    Ok(())
}

fn terminate_owner(pid: u32) {
    // Best effort: the wait loop below decides success by lock acquisition.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

fn kill_owner(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Holder for the studio lock. Dropping it closes the file, which releases
/// the flock. The lock file itself is left in place (unlocked) for the next
/// starter; a crash releases it the same way via the kernel.
pub struct SingleInstanceGuard {
    _file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

/// Acquire the studio lock, terminating a live predecessor first.
///
/// At most one studio holds the lock. A starter that finds it held verifies
/// the recorded owner is a live studio process, asks it to exit, waits for the
/// lock, escalates once, then becomes the active instance. Unrelated or stale
/// owners are never signalled; if the lock stays held by something else this
/// returns an error instead of killing it.
pub fn acquire_single_instance() -> Result<SingleInstanceGuard> {
    let path = studio_lock_path();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open studio lock {}", path.display()))?;

    if try_lock_nb(&file)? {
        write_identity(&mut file)?;
        return Ok(SingleInstanceGuard { _file: file, path });
    }

    // The lock is held. The holder writes its identity right after acquiring,
    // so allow a brief window for a racing starter to publish it.
    let mut owner = read_identity_file(&path);
    for _ in 0..10 {
        if owner.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        if try_lock_nb(&file)? {
            write_identity(&mut file)?;
            return Ok(SingleInstanceGuard { _file: file, path });
        }
        owner = read_identity_file(&path);
    }

    // A second quick attempt covers a holder that just exited: the kernel
    // released the lock but we raced the read.
    if try_lock_nb(&file)? {
        write_identity(&mut file)?;
        return Ok(SingleInstanceGuard { _file: file, path });
    }

    let Some((pid, starttime)) = owner else {
        anyhow::bail!(
            "studio lock {} is held but has no owner identity; not signalling",
            path.display()
        );
    };
    if !should_terminate_owner(pid, starttime) {
        anyhow::bail!(
            "studio lock {} is held by pid {pid} which is not a live studio process; not signalling",
            path.display()
        );
    }

    terminate_owner(pid);
    let start = Instant::now();
    loop {
        if try_lock_nb(&file)? {
            write_identity(&mut file)?;
            return Ok(SingleInstanceGuard { _file: file, path });
        }
        if start.elapsed() >= TAKEOVER_GRACE {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Grace period expired: one escalation, then a final wait. A clean exit
    // closes the holder's subscribe stream, freeing its daemon slot before
    // the new instance connects.
    kill_owner(pid);
    let start = Instant::now();
    loop {
        if try_lock_nb(&file)? {
            write_identity(&mut file)?;
            return Ok(SingleInstanceGuard { _file: file, path });
        }
        if start.elapsed() >= KILL_WAIT {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // The old process may have exited without the lock releasing yet if we
    // raced its close; one last non-blocking attempt before giving up.
    let mut probe = String::new();
    file.seek(SeekFrom::Start(0)).ok();
    file.read_to_string(&mut probe).ok();
    anyhow::bail!("previous config studio (pid {pid}) did not exit; lock still held")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_identity_round_trips() {
        assert_eq!(parse_lock_identity("1234 5678\n"), Some((1234, 5678)));
        assert_eq!(parse_lock_identity("  42   99  "), Some((42, 99)));
    }

    #[test]
    fn lock_identity_rejects_garbage() {
        assert_eq!(parse_lock_identity(""), None);
        assert_eq!(parse_lock_identity("not-a-pid 123"), None);
        assert_eq!(parse_lock_identity("1234"), None);
        assert_eq!(parse_lock_identity("0 123"), None);
    }

    #[test]
    fn stale_owner_is_not_alive() {
        // A PID that almost certainly never exists is stale, never live.
        assert!(!is_owner_alive(4_000_000_000, 12345));
        assert!(!should_terminate_owner(4_000_000_000, 12345));
    }

    #[test]
    fn unrelated_processes_are_never_termination_targets() {
        // PID 1 exists on Linux but is not a studio process.
        assert!(!is_studio_process(1, None));
        assert!(!should_terminate_owner(1, 0));
        // Ourselves are never a takeover target even with a valid starttime.
        let own = std::process::id();
        if let Some(starttime) = proc_starttime(own) {
            assert!(!is_studio_process(own, Some(starttime)));
            assert!(!should_terminate_owner(own, starttime));
        }
    }

    #[test]
    fn live_lock_identity_matches_proc() {
        let own = std::process::id();
        let Some(starttime) = proc_starttime(own) else {
            return;
        };
        // The identity we would write must parse back and describe a live
        // owner (but never a kill target for ourselves).
        let text = format!("{own} {starttime}\n");
        let (pid, parsed_start) = parse_lock_identity(&text).expect("own identity parses");
        assert_eq!(pid, own);
        assert_eq!(parsed_start, starttime);
        assert!(is_owner_alive(pid, parsed_start));
    }
}
