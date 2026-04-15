#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_vorto");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
#[ignore = "requires Linux, root, and the nft command"]
fn nft_mode_process_lifecycle_cleans_rules_and_forwards_tcp() {
    if !nft_test_ready() {
        eprintln!(
            "Skipping nft lifecycle integration test because nft/root prerequisites are unavailable."
        );
        return;
    }

    let before_tables = list_vorto_tables().expect("should list nft tables before the test");
    let backend = EchoServer::spawn().expect("echo backend should start");
    let listen = reserve_tcp_addr();
    let config_dir = TestDir::new("nft-lifecycle");
    write_config(config_dir.path(), listen, backend.addr).expect("config should be writable");

    let mut process = VortoProcess::spawn(config_dir.path()).expect("vorto process should start");
    let table_name = wait_for_new_table(&before_tables, STARTUP_TIMEOUT)
        .expect("vorto should create an nft table");
    process.table_name = Some(table_name.clone());

    wait_for_echo(listen, b"nft-lifecycle", STARTUP_TIMEOUT)
        .expect("nft mode should forward traffic while the process is running");

    process
        .terminate()
        .expect("vorto process should exit cleanly on SIGTERM");
    wait_for_table_gone(&table_name, SHUTDOWN_TIMEOUT)
        .expect("nft table should be removed after process shutdown");

    drop(backend);
}

#[test]
#[ignore = "requires Linux, root, nft, and local iperf3 for manual throughput benchmarking"]
fn nft_mode_iperf3_throughput() {
    if !nft_test_ready() {
        eprintln!("Skipping nft iperf3 test because nft/root prerequisites are unavailable.");
        return;
    }
    if !command_succeeds("iperf3", ["--version"]) {
        eprintln!("Skipping nft iperf3 test because iperf3 is unavailable.");
        return;
    }

    let seconds = std::env::var("VORTO_NFT_IPERF_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5);
    let parallel = std::env::var("VORTO_NFT_IPERF_PARALLEL")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(4);
    let min_gbps = std::env::var("VORTO_NFT_IPERF_MIN_GBPS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok());

    let backend = reserve_tcp_addr();
    let listen = reserve_tcp_addr();
    let config_dir = TestDir::new("nft-iperf");
    write_config(config_dir.path(), listen, backend).expect("config should be writable");

    let before_tables = list_vorto_tables().expect("should list nft tables before iperf");
    let mut process = VortoProcess::spawn(config_dir.path()).expect("vorto process should start");
    let table_name = wait_for_new_table(&before_tables, STARTUP_TIMEOUT)
        .expect("vorto should create an nft table");
    process.table_name = Some(table_name.clone());

    let direct = run_iperf_round(backend.port(), backend.port(), seconds, parallel, false)
        .expect("direct iperf run should succeed");
    let tunneled = run_iperf_round(backend.port(), listen.port(), seconds, parallel, true)
        .expect("nft iperf run should succeed");

    println!(
        "nft iperf3 direct: {:.2} Gbps, tunneled: {:.2} Gbps",
        direct / 1_000_000_000.0,
        tunneled / 1_000_000_000.0
    );

    if let Some(min_gbps) = min_gbps {
        assert!(
            tunneled >= min_gbps * 1_000_000_000.0,
            "tunneled throughput {:.2} Gbps is below the requested floor {:.2} Gbps",
            tunneled / 1_000_000_000.0,
            min_gbps
        );
    }

    process
        .terminate()
        .expect("vorto process should exit cleanly after iperf");
    wait_for_table_gone(&table_name, SHUTDOWN_TIMEOUT)
        .expect("nft table should be removed after iperf shutdown");
}

fn nft_test_ready() -> bool {
    is_root() && command_succeeds("nft", ["--version"])
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn command_succeeds<const N: usize>(program: &str, args: [&str; N]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn reserve_tcp_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral TCP bind should work");
    let addr = listener
        .local_addr()
        .expect("ephemeral TCP addr should exist");
    drop(listener);
    addr
}

fn write_config(dir: &Path, listen: SocketAddr, target: SocketAddr) -> io::Result<()> {
    let config = format!(
        "daemon_log: false\nmode: nft\n\ntunnels:\n  - name: nft-test\n    enabled: true\n    protocol: tcp\n    target: {target}\n    listen: {listen}\n"
    );
    fs::write(dir.join("config.yaml"), config)
}

fn list_vorto_tables() -> io::Result<BTreeSet<String>> {
    let output = Command::new("nft")
        .args(["list", "tables"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "nft list tables failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let mut tables = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        if matches!(parts.next(), Some("table"))
            && matches!(parts.next(), Some("ip"))
            && let Some(name) = parts.next()
            && name.starts_with("vorto_nat_")
        {
            tables.insert(name.to_string());
        }
    }
    Ok(tables)
}

fn wait_for_new_table(before: &BTreeSet<String>, timeout: Duration) -> io::Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let current = list_vorto_tables()?;
        if let Some(name) = current.difference(before).next() {
            return Ok(name.clone());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for nft table creation",
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_table_gone(table_name: &str, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let current = list_vorto_tables()?;
        if !current.contains(table_name) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for nft table '{table_name}' to disappear"),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_echo(addr: SocketAddr, payload: &[u8], timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match echo_round_trip(addr, payload) {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

fn echo_round_trip(addr: SocketAddr, payload: &[u8]) -> io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(payload)?;
    let mut echoed = vec![0_u8; payload.len()];
    stream.read_exact(&mut echoed)?;
    if echoed == payload {
        Ok(())
    } else {
        Err(io::Error::other("echo response did not match the payload"))
    }
}

fn run_iperf_round(
    server_port: u16,
    client_port: u16,
    seconds: u64,
    parallel: u32,
    expect_tunnel: bool,
) -> io::Result<f64> {
    let mut server = IperfServer::spawn(server_port)?;
    thread::sleep(Duration::from_millis(300));

    let output = Command::new("iperf3")
        .args([
            "-c",
            "127.0.0.1",
            "-p",
            &client_port.to_string(),
            "-t",
            &seconds.to_string(),
            "-P",
            &parallel.to_string(),
            "-J",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    server.wait()?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "iperf3 client failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let root: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse iperf3 JSON output: {error}"),
        )
    })?;
    let bits_per_second = root["end"]["sum_received"]["bits_per_second"]
        .as_f64()
        .or_else(|| root["end"]["sum_sent"]["bits_per_second"].as_f64())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "iperf3 JSON missing throughput")
        })?;

    if expect_tunnel && bits_per_second <= 0.0 {
        return Err(io::Error::other(
            "nft tunnel iperf3 throughput must be positive",
        ));
    }

    Ok(bits_per_second)
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(prefix: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("vorto-{prefix}-{unique}"));
        fs::create_dir_all(&path).expect("temporary test directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct EchoServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EchoServer {
    fn spawn() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                            let mut buffer = [0_u8; 16 * 1024];
                            loop {
                                let read = match stream.read(&mut buffer) {
                                    Ok(0) => break,
                                    Ok(read) => read,
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                        ) =>
                                    {
                                        continue;
                                    }
                                    Err(_) => break,
                                };
                                if stream.write_all(&buffer[..read]).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct IperfServer {
    child: Child,
}

impl IperfServer {
    fn spawn(port: u16) -> io::Result<Self> {
        let child = Command::new("iperf3")
            .args(["-s", "-1", "-p", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self { child })
    }

    fn wait(&mut self) -> io::Result<()> {
        let status = self.child.wait()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "iperf3 server exited with {status}"
            )))
        }
    }
}

impl Drop for IperfServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct VortoProcess {
    child: Option<Child>,
    table_name: Option<String>,
}

impl VortoProcess {
    fn spawn(config_dir: &Path) -> io::Result<Self> {
        let child = Command::new(BIN)
            .arg("run")
            .current_dir(config_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Self {
            child: Some(child),
            table_name: None,
        })
    }

    fn terminate(&mut self) -> io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };

        let pid = child.id() as i32;
        let signal_result = unsafe { libc::kill(pid, libc::SIGTERM) };
        if signal_result != 0 {
            return Err(io::Error::last_os_error());
        }

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            if child.try_wait()?.is_some() {
                self.child.take();
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for vorto process {pid} to exit"),
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for VortoProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Some(table_name) = self.table_name.take() {
            let _ = Command::new("nft")
                .args(["delete", "table", "ip", &table_name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
