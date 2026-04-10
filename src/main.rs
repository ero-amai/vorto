mod config;
mod interactive;
mod runtime;
mod version;

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use config::AppConfig;

type AppResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type ProcessId = u32;

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
        Some("daemon") => start_daemon(&cwd, &config_path, &pid_path, &log_path),
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

fn start_daemon(cwd: &Path, config_path: &Path, pid_path: &Path, log_path: &Path) -> AppResult<()> {
    if let Some(pid) = read_pid(pid_path)? {
        if daemon_process_matches(pid)? {
            println!("Daemon is already running with PID {}.", pid);
            return Ok(());
        }
        let _ = fs::remove_file(pid_path);
    }

    let config = AppConfig::load_for_runtime(config_path)?;

    let (stdout, stderr) = daemon_stdio(config.daemon_log, log_path)?;

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
    if config.daemon_log {
        println!(
            "Daemon started with PID {}. Log file: {}",
            child.id(),
            log_path.display()
        );
    } else {
        println!("Daemon started with PID {}. Log file output is disabled.", child.id());
    }
    Ok(())
}

fn daemon_stdio(log_enabled: bool, log_path: &Path) -> AppResult<(Stdio, Stdio)> {
    if log_enabled {
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let stderr = stdout.try_clone()?;
        Ok((Stdio::from(stdout), Stdio::from(stderr)))
    } else {
        let stdout = File::open(null_device_path())?;
        let stderr = stdout.try_clone()?;
        Ok((Stdio::from(stdout), Stdio::from(stderr)))
    }
}

fn null_device_path() -> &'static str {
    #[cfg(windows)]
    {
        "NUL"
    }

    #[cfg(not(windows))]
    {
        "/dev/null"
    }
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

fn read_pid(path: &Path) -> AppResult<Option<ProcessId>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)?;
    let pid = parse_pid(&content)?;
    Ok(Some(pid))
}

#[cfg(target_os = "linux")]
fn daemon_process_matches(pid: ProcessId) -> AppResult<bool> {
    let proc_dir = std::path::PathBuf::from(format!("/proc/{}", pid));
    if !proc_dir.exists() {
        return Ok(false);
    }

    let cmdline = fs::read(proc_dir.join("cmdline"))?;
    let binary_name = current_binary_name()?;
    Ok(is_daemon_worker_cmdline(&cmdline, &binary_name))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn daemon_process_matches(pid: ProcessId) -> AppResult<bool> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }

    let command_line = String::from_utf8_lossy(&output.stdout);
    let binary_name = current_binary_name()?;
    Ok(is_daemon_worker_command(command_line.trim(), &binary_name))
}

#[cfg(not(unix))]
fn daemon_process_matches(pid: ProcessId) -> AppResult<bool> {
    let _ = pid;
    Ok(false)
}

fn terminate_process(pid: ProcessId) -> AppResult<()> {
    #[cfg(unix)]
    {
        let pid = platform_pid(pid)?;
        unsafe {
            if libc::kill(pid, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(())
        }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(io::Error::other("The stop command is not supported on this platform.").into())
    }
}

fn parse_pid(content: &str) -> AppResult<ProcessId> {
    content.trim().parse::<ProcessId>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Invalid PID file contents: {}", error),
        )
        .into()
    })
}

#[cfg(unix)]
fn platform_pid(pid: ProcessId) -> AppResult<libc::pid_t> {
    i32::try_from(pid).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("PID {} is too large for platform APIs.", pid),
        )
        .into()
    })
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

#[cfg(all(unix, not(target_os = "linux")))]
fn is_daemon_worker_command(command: &str, binary_name: &str) -> bool {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return false;
    };
    let Some(worker_arg) = parts.next() else {
        return false;
    };

    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        == Some(binary_name)
        && worker_arg == DAEMON_WORKER
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{is_daemon_worker_cmdline, platform_pid};
    #[cfg(all(unix, not(target_os = "linux")))]
    use super::is_daemon_worker_command;
    use super::parse_pid;

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

    #[test]
    fn parse_pid_accepts_unsigned_process_ids() {
        assert_eq!(parse_pid("12345\n").expect("pid should parse"), 12_345);
    }

    #[cfg(unix)]
    #[test]
    fn platform_pid_rejects_values_outside_pid_t_range() {
        let error = platform_pid(u32::MAX).expect_err("oversized pid should fail");
        assert!(error.to_string().contains("too large"));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn daemon_worker_command_matches_expected_process() {
        assert!(is_daemon_worker_command("/tmp/vorto __daemon_worker", "vorto"));
        assert!(!is_daemon_worker_command("/usr/bin/python3 server.py", "vorto"));
        assert!(!is_daemon_worker_command("/tmp/vorto run", "vorto"));
    }
}
