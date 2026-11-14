use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process,
    sync::{LazyLock, Mutex},
    thread,
    time::Duration,
};

use serde::Deserialize;
use systemd::sd_journal_log;

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
static VALUES: LazyLock<ValueString> = LazyLock::new(|| {
    let (boost, revert): (Vec<String>, Vec<String>) =
        fs::read_to_string("/sys/fs/cgroup/dmem.capacity")
            .expect("Could not read GPU devices")
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
                let capacity = capacity.parse::<u64>();
                if let Ok(capacity) = capacity {
                    Some((format!("{gpu} {capacity}\n"), format!("{gpu} 0\n")))
                } else {
                    sd_journal_log!(4, "Failed to get capacity for device: {gpu}").unwrap_or(());
                    None
                }
            })
            .collect();

    if boost.is_empty() || revert.is_empty() {
        sd_journal_log!(3, "Could not find any GPUs").unwrap_or(());
        process::exit(1);
    }
    ValueString { boost, revert }
});

fn main() {
    ctrlc::set_handler(|| {
        if let Ok(prev_lock) = PREVIOUS.lock() {
            if let Some(previous) = *prev_lock {
                write_cgroup_dmem(previous, OP::Revert);
            } else {
                sd_journal_log!(3, "Failed to revert previous PID dmem.low value").unwrap_or(());
            }
            process::exit(0);
        }
    })
    .inspect_err(|e| {
        sd_journal_log!(3, "{e}").unwrap_or(());
    })
    .expect("Failed to register interrupt handler");

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

    let trimmed = response.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(None);
    }

    let active: ActiveWindow = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    Ok(active.pid.filter(|&pid| pid > 0))
}

fn sync_active_boost(cmd_sock: &Path) {
    let Ok(mut prev_lock) = PREVIOUS.lock() else {
        sd_journal_log!(4, "Failed to acquire lock on previous boosted pid").unwrap_or(());
        return;
    };

    match get_active_pid(cmd_sock) {
        Ok(current) => match (*prev_lock, current) {
            (None, None) => (),
            (None, Some(pid)) => {
                write_cgroup_dmem(pid, OP::Boost);
                *prev_lock = Some(pid);
            }
            (Some(previous), None) => {
                write_cgroup_dmem(previous, OP::Revert);
                *prev_lock = None;
            }
            (Some(previous), Some(pid)) => {
                if previous != pid {
                    write_cgroup_dmem(previous, OP::Revert);
                    write_cgroup_dmem(pid, OP::Boost);
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

fn write_cgroup_dmem(pid: i32, op: OP) {
    match systemd::login::get_cgroup(Some(pid)) {
        Ok(service) => {
            let path = format!("/sys/fs/cgroup{}/dmem.low", service);
            if path.contains("app.slice") {
                // dmemcg-booster might not be ready yet so retry few times before logging an error
                let mut retry = 0;
                let res = loop {
                    let write: io::Result<()> = match op {
                        OP::Boost => &VALUES.boost,
                        OP::Revert => &VALUES.revert,
                    }
                    .iter()
                    .try_for_each(|value| fs::write(&path, value));

                    retry += 1;
                    if retry >= RETRY_COUNT || write.is_ok() {
                        break write;
                    }
                    sd_journal_log!(
                        5,
                        "Could not write to dmem.low. Retrying {retry} / {RETRY_COUNT}."
                    )
                    .unwrap_or(());
                    thread::sleep(SLEEP_DURATION);
                };
                if let Err(err) = res {
                    sd_journal_log!(3, "Error: {err} for path: {path}.").unwrap_or(());
                };
            }
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
