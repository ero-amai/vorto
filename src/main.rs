mod config;
mod interactive;
mod runtime;
mod version;

use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use config::AppConfig;

type AppResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const CONFIG_FILE: &str = "config.yaml";
const PID_FILE: &str = "vorto.pid";
const LOG_FILE: &str = "vorto.log";
const DEFAULT_POLL_SECS: u64 = 2;
const DAEMON_WORKER: &str = "__daemon_worker";

#[tokio::main]
async fn main() {
    if let Err(error) = entry().await {
        eprintln!("{}", error);
        std::process::exit(1);
    }
}

async fn entry() -> AppResult<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let cwd = env::current_dir()?;
    let config_path = cwd.join(CONFIG_FILE);
    let pid_path = cwd.join(PID_FILE);
    let log_path = cwd.join(LOG_FILE);

    match args.first().map(String::as_str) {
        Some("config") | Some("create") => interactive::manage_config(&config_path),
        Some("run") => runtime::run_foreground(&config_path).await,
        Some("daemon") => start_daemon(&cwd, &pid_path, &log_path),
        Some("stop") => stop_daemon(&pid_path),
        Some("status") => print_status(&pid_path),
        Some("version") | Some("-V") | Some("--version") => {
            version::print();
            Ok(())
        }
        Some("help") | Some("-h") | Some("--help") => {
            print_help();
            Ok(())
        }
        Some(DAEMON_WORKER) => {
            let _pid_guard = PidFileGuard::new(&pid_path)?;
            runtime::run_config_watcher(&config_path, Duration::from_secs(DEFAULT_POLL_SECS)).await
        }
        _ => {
            print_help();
            Ok(())
        }
    }
}

fn start_daemon(cwd: &Path, pid_path: &Path, log_path: &Path) -> AppResult<()> {
    if let Some(pid) = read_pid(pid_path)? {
        if daemon_process_matches(pid)? {
            println!("Daemon is already running with PID {}.", pid);
            return Ok(());
        }
        let _ = fs::remove_file(pid_path);
    }

    AppConfig::load_for_runtime(&cwd.join(CONFIG_FILE))?;

    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let stderr = stdout.try_clone()?;

    let exe = env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .current_dir(cwd)
        .arg(DAEMON_WORKER)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = command.spawn()?;
    fs::write(pid_path, format!("{}\n", child.id()))?;
    println!(
        "Daemon started with PID {}. Log file: {}",
        child.id(),
        log_path.display()
    );
    Ok(())
}

fn stop_daemon(pid_path: &Path) -> AppResult<()> {
    let Some(pid) = read_pid(pid_path)? else {
        println!("Daemon is not running.");
        return Ok(());
    };

    if !daemon_process_matches(pid)? {
        let _ = fs::remove_file(pid_path);
        println!("The PID file does not point to a running daemon for this executable.");
        return Ok(());
    }

    terminate_process(pid)?;
    println!("Sent a stop signal to PID {}.", pid);
    Ok(())
}

fn print_status(pid_path: &Path) -> AppResult<()> {
    let Some(pid) = read_pid(pid_path)? else {
        println!("Daemon is not running.");
        return Ok(());
    };

    if daemon_process_matches(pid)? {
        println!("Daemon is running with PID {}.", pid);
    } else {
        println!("The PID file does not point to a running daemon for this executable.");
    }

    Ok(())
}

fn print_help() {
    println!("Usage:");
    println!("  ./vorto config   Interactively manage tunnels in ./config.yaml");
    println!("  ./vorto run      Run in the foreground (./config.yaml)");
    println!("  ./vorto daemon   Run in the background (hot-reload ./config.yaml changes)");
    println!("  ./vorto stop     Stop the background process");
    println!("  ./vorto status   Show background process status");
    println!("  ./vorto version  Show release, commit, and repository information");
    println!("  ./vorto help     Show this help message");
}

fn read_pid(path: &Path) -> AppResult<Option<i32>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)?;
    let pid = content.trim().parse::<i32>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Invalid PID file contents: {}", error),
        )
    })?;
    Ok(Some(pid))
}

fn daemon_process_matches(pid: i32) -> AppResult<bool> {
    #[cfg(unix)]
    {
        let proc_dir = PathBuf::from(format!("/proc/{}", pid));
        if !proc_dir.exists() {
            return Ok(false);
        }

        let cmdline = fs::read(proc_dir.join("cmdline"))?;
        let binary_name = current_binary_name()?;
        Ok(is_daemon_worker_cmdline(&cmdline, &binary_name))
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(false)
    }
}

fn terminate_process(pid: i32) -> AppResult<()> {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid, libc::SIGTERM) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(io::Error::other("The stop command is not supported on this platform.").into())
    }
}

struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn new(path: &Path) -> AppResult<Self> {
        fs::write(path, format!("{}\n", std::process::id()))?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn current_binary_name() -> AppResult<String> {
    env::current_exe()?
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| io::Error::other("Could not determine the current executable name.").into())
}

#[cfg(unix)]
fn is_daemon_worker_cmdline(cmdline: &[u8], binary_name: &str) -> bool {
    let parts = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    matches!(parts.as_slice(), [program, worker_arg, ..]
        if daemon_program_name(program, binary_name) && *worker_arg == DAEMON_WORKER.as_bytes())
}

#[cfg(unix)]
fn daemon_program_name(program: &[u8], binary_name: &str) -> bool {
    Path::new(OsStr::from_bytes(program))
        .file_name()
        .and_then(|name| name.to_str())
        == Some(binary_name)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::is_daemon_worker_cmdline;

    #[cfg(unix)]
    #[test]
    fn daemon_worker_cmdline_matches_expected_process() {
        assert!(is_daemon_worker_cmdline(
            b"/tmp/vorto\0__daemon_worker\0",
            "vorto"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_worker_cmdline_rejects_other_processes() {
        assert!(!is_daemon_worker_cmdline(
            b"/usr/bin/python3\0server.py\0",
            "vorto"
        ));
        assert!(!is_daemon_worker_cmdline(
            b"/tmp/rttf\0__daemon_worker\0",
            "vorto"
        ));
        assert!(!is_daemon_worker_cmdline(b"/tmp/vorto\0run\0", "vorto"));
    }
}
