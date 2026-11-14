use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use serde::Deserialize;
use systemd::sd_journal_log;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OP {
    Boost,
    Revert,
}

struct ValueString {
    boost: Vec<String>,
    revert: Vec<String>,
}

const SLEEP_DURATION: Duration = Duration::from_millis(500);
const RETRY_COUNT: i32 = 5;

const VRAM_CGROUP_NAMES: [&str; 2] = ["vram", "vidmem"];

static PREVIOUS: LazyLock<Mutex<Option<i32>>> = LazyLock::new(|| Mutex::new(None));
static BOOST_GENERATION: AtomicU64 = AtomicU64::new(0);
static VALUES: LazyLock<ValueString> = LazyLock::new(|| {
    let capacity_file = match fs::read_to_string("/sys/fs/cgroup/dmem.capacity") {
        Ok(content) => content,
        Err(err) => {
            sd_journal_log!(3, "Could not read GPU devices: {err}").unwrap_or(());
            process::exit(1);
        }
    };
    let Some(values) = parse_dmem_capacity(&capacity_file) else {
        sd_journal_log!(3, "Could not find any GPUs").unwrap_or(());
        process::exit(1);
    };
    values
});

fn parse_dmem_capacity(capacity_file: &str) -> Option<ValueString> {
    let (boost, revert): (Vec<String>, Vec<String>) = capacity_file
        .lines()
        .filter_map(|line| {
            if !VRAM_CGROUP_NAMES
                .into_iter()
                .any(|name| line.contains(name))
            {
                return None;
            }

            let [gpu, capacity] = line.split_whitespace().collect::<Vec<&str>>()[..] else {
                return None;
            };
            if let Ok(capacity) = capacity.parse::<u64>() {
                Some((format!("{gpu} {capacity}\n"), format!("{gpu} 0\n")))
            } else {
                sd_journal_log!(4, "Failed to get capacity for device: {gpu}").unwrap_or(());
                None
            }
        })
        .collect();

    if boost.is_empty() || revert.is_empty() {
        None
    } else {
        Some(ValueString { boost, revert })
    }
}

fn main() {
    if let Err(err) = ctrlc::set_handler(|| {
        if let Ok(prev_lock) = PREVIOUS.lock() {
            if let Some(previous) = *prev_lock {
                write_cgroup_dmem(previous, OP::Revert);
            }
            process::exit(0);
        }
    }) {
        sd_journal_log!(3, "Failed to register interrupt handler: {err}").unwrap_or(());
        process::exit(1);
    }

    let (cmd_sock, event_sock) = match hyprland_sockets() {
        Ok(paths) => paths,
        Err(err) => {
            sd_journal_log!(3, "{err}").unwrap_or(());
            process::exit(1);
        }
    };

    sync_active_boost(&cmd_sock);

    if let Err(err) = listen_active_window_events(&event_sock, &cmd_sock) {
        sd_journal_log!(3, "Event listener stopped: {err}").unwrap_or(());
        if let Ok(Some(prev)) = PREVIOUS.lock().map(|p| *p) {
            write_cgroup_dmem(prev, OP::Revert);
        }
        process::exit(1);
    }

    if let Ok(Some(prev)) = PREVIOUS.lock().map(|p| *p) {
        write_cgroup_dmem(prev, OP::Revert);
    }
}

#[derive(Deserialize)]
struct ActiveWindow {
    pid: Option<i32>,
}

fn hyprland_sockets() -> io::Result<(PathBuf, PathBuf)> {
    let runtime_dir = env::var("XDG_RUNTIME_DIR")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HYPRLAND_INSTANCE_SIGNATURE is not set (is Hyprland running?)",
        )
    })?;
    let base = Path::new(&runtime_dir).join("hypr").join(sig);
    Ok((base.join(".socket.sock"), base.join(".socket2.sock")))
}

fn parse_active_window_pid(raw: &str) -> Result<Option<i32>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(None);
    }

    let active: ActiveWindow = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    Ok(active.pid.filter(|&pid| pid > 0))
}

fn get_active_pid(cmd_sock: &Path) -> Result<Option<i32>, String> {
    let mut stream = UnixStream::connect(cmd_sock).map_err(|e| e.to_string())?;
    stream
        .write_all(b"j/activewindow")
        .map_err(|e| e.to_string())?;
    let _ = stream.shutdown(Shutdown::Write);

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| e.to_string())?;

    parse_active_window_pid(&response)
}

fn sync_active_boost(cmd_sock: &Path) {
    let mut prev_lock = PREVIOUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let generation = BOOST_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    match get_active_pid(cmd_sock) {
        Ok(current) => match (*prev_lock, current) {
            (None, None) => (),
            (None, Some(pid)) => {
                write_cgroup_dmem_with_gen(pid, OP::Boost, generation);
                *prev_lock = Some(pid);
            }
            (Some(previous), None) => {
                write_cgroup_dmem(previous, OP::Revert);
                *prev_lock = None;
            }
            (Some(previous), Some(pid)) => {
                if previous != pid {
                    write_cgroup_dmem(previous, OP::Revert);
                    write_cgroup_dmem_with_gen(pid, OP::Boost, generation);
                    *prev_lock = Some(pid);
                }
            }
        },
        Err(err) => {
            if let Some(previous) = *prev_lock {
                write_cgroup_dmem(previous, OP::Revert);
                *prev_lock = None;
            }
            sd_journal_log!(4, "Failed to get active window: {err}").unwrap_or(());
        }
    }
}

fn listen_active_window_events(event_sock: &Path, cmd_sock: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(event_sock)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Hyprland event socket closed",
            ));
        }
        if line.starts_with("activewindowv2>>") {
            sync_active_boost(cmd_sock);
        }
    }
}

fn write_dmem_values(path: &str, op: OP) -> io::Result<()> {
    match op {
        OP::Boost => &VALUES.boost,
        OP::Revert => &VALUES.revert,
    }
    .iter()
    .try_for_each(|value| fs::write(path, value))
}

fn write_cgroup_dmem(pid: i32, op: OP) {
    let generation = BOOST_GENERATION.load(Ordering::SeqCst);
    write_cgroup_dmem_with_gen(pid, op, generation);
}

fn write_cgroup_dmem_with_gen(pid: i32, op: OP, generation: u64) {
    match systemd::login::get_cgroup(Some(pid)) {
        Ok(service) => {
            let path = format!("/sys/fs/cgroup{service}/dmem.low");
            if !path.contains("app.slice") {
                return;
            }

            if write_dmem_values(&path, op).is_ok() {
                return;
            }

            if op == OP::Revert {
                // Synchronous single-path revert on focus loss / shutdown.
                if let Err(err) = write_dmem_values(&path, OP::Revert) {
                    sd_journal_log!(3, "Error: {err} for path: {path}.").unwrap_or(());
                }
                return;
            }

            // dmemcg-booster might not be ready yet; retry asynchronously so the Hyprland
            // event loop is never blocked, cancelling immediately if focus changes again.
            thread::spawn(move || {
                for retry in 1..RETRY_COUNT {
                    thread::sleep(SLEEP_DURATION);
                    if BOOST_GENERATION.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    sd_journal_log!(
                        5,
                        "Could not write to dmem.low. Retrying {retry} / {RETRY_COUNT}."
                    )
                    .unwrap_or(());
                    if let Err(err) = write_dmem_values(&path, OP::Boost) {
                        if retry + 1 >= RETRY_COUNT {
                            sd_journal_log!(3, "Error: {err} for path: {path}.").unwrap_or(());
                        }
                    } else {
                        return;
                    }
                }
            });
        }
        Err(e) => {
            // Skip writing to journal when previous process no longer exists.
            if !e.to_string().contains("No such process") {
                sd_journal_log!(
                    4,
                    "Error when trying to find cgroup by pid: {pid} with error: {e}"
                )
                .unwrap_or(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dmem_capacity_extracts_vram_and_vidmem() {
        let input = "\
nvidia/00000000:01:00.0/vidmem 8546942976
amdgpu/0000:03:00.0/vram 8589934592
amdgpu/0000:03:00.0/gtt 16777216000
";
        let parsed = parse_dmem_capacity(input).expect("should parse GPU VRAM lines");
        assert_eq!(
            parsed.boost,
            vec![
                "nvidia/00000000:01:00.0/vidmem 8546942976\n",
                "amdgpu/0000:03:00.0/vram 8589934592\n",
            ]
        );
        assert_eq!(
            parsed.revert,
            vec![
                "nvidia/00000000:01:00.0/vidmem 0\n",
                "amdgpu/0000:03:00.0/vram 0\n",
            ]
        );
    }

    #[test]
    fn parse_dmem_capacity_ignores_non_vram_and_invalid() {
        let input = "\
amdgpu/0000:03:00.0/gtt 16777216000
nvidia/00000000:01:00.0/vidmem not_a_number
";
        assert!(parse_dmem_capacity(input).is_none());
    }

    #[test]
    fn parse_active_window_pid_handles_valid_empty_and_null() {
        assert_eq!(parse_active_window_pid(r#"{"pid": 4242}"#), Ok(Some(4242)));
        assert_eq!(parse_active_window_pid("{}"), Ok(None));
        assert_eq!(parse_active_window_pid("   "), Ok(None));
        assert_eq!(parse_active_window_pid(r#"{"pid": null}"#), Ok(None));
        assert_eq!(parse_active_window_pid(r#"{"pid": 0}"#), Ok(None));
    }

    #[test]
    fn parse_active_window_pid_rejects_invalid_json() {
        assert!(parse_active_window_pid("{invalid").is_err());
    }
}
