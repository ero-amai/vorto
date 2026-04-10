use std::collections::{HashMap, hash_map::Entry};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::copy_bidirectional_with_sizes;
#[cfg(target_os = "linux")]
use tokio::io::Interest;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

#[cfg(target_os = "linux")]
use std::net::Shutdown;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::AppResult;
use crate::config::{AppConfig, TcpMode, TunnelConfig};

const UDP_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const UDP_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const TCP_COPY_BUFFER_SIZE: usize = 256 * 1024;
#[cfg(target_os = "linux")]
const SPLICE_CHUNK_SIZE: usize = 256 * 1024;
#[cfg(target_os = "linux")]
const SPLICE_PIPE_SIZE: libc::c_int = 1_048_576;

#[derive(Default)]
pub struct TunnelManager {
    active: HashMap<String, ActiveTunnel>,
}

struct ActiveTunnel {
    spec: TunnelConfig,
    stop_tx: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

struct UdpSession {
    upstream: Arc<UdpSocket>,
    last_seen: Instant,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl TunnelManager {
    pub async fn start_from_config(&mut self, config: &AppConfig) -> bool {
        self.reconcile(config.enabled_tunnels()).await
    }

    pub async fn reconcile(&mut self, desired_tunnels: Vec<TunnelConfig>) -> bool {
        let desired_map = desired_tunnels
            .into_iter()
            .map(|tunnel| (tunnel.name.clone(), tunnel))
            .collect::<HashMap<_, _>>();
        let mut all_started = true;

        let mut to_remove = Vec::new();
        for (name, active) in &self.active {
            match desired_map.get(name) {
                Some(next) if *next == active.spec => {}
                _ => to_remove.push(name.clone()),
            }
        }

        for name in to_remove {
            if let Some(active) = self.active.remove(&name) {
                active.stop().await;
            }
        }

        for (name, tunnel) in desired_map {
            let should_start = match self.active.get(&name) {
                Some(active) => active.spec != tunnel,
                None => true,
            };

            if should_start {
                match ActiveTunnel::spawn(tunnel.clone()).await {
                    Ok(active) => {
                        self.active.insert(name, active);
                    }
                    Err(error) => {
                        all_started = false;
                        eprintln!("Failed to start tunnel '{}': {}", tunnel.name, error);
                    }
                }
            }
        }

        all_started
    }

    pub async fn stop_all(&mut self) {
        let names = self.active.keys().cloned().collect::<Vec<_>>();
        for name in names {
            if let Some(active) = self.active.remove(&name) {
                active.stop().await;
            }
        }
    }
}

impl ActiveTunnel {
    async fn spawn(spec: TunnelConfig) -> AppResult<Self> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut handles = Vec::new();

        if spec.protocol.supports_tcp() {
            let tcp_listener = TcpListener::bind(&spec.listen).await?;
            let tcp_spec = spec.clone();
            let tcp_stop = stop_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = run_tcp_tunnel(tcp_spec, tcp_listener, tcp_stop).await {
                    eprintln!("TCP tunnel stopped: {}", error);
                }
            }));
        }

        if spec.protocol.supports_udp() {
            let udp_socket = Arc::new(UdpSocket::bind(&spec.listen).await?);
            let udp_spec = spec.clone();
            let udp_stop = stop_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = run_udp_tunnel(udp_spec, udp_socket, udp_stop).await {
                    eprintln!("UDP tunnel stopped: {}", error);
                }
            }));
        }

        Ok(Self {
            spec,
            stop_tx,
            handles,
        })
    }

    async fn stop(self) {
        let _ = self.stop_tx.send(true);
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

pub async fn run_foreground(config_path: &Path) -> AppResult<()> {
    let config = AppConfig::load_for_runtime(config_path)?;
    let mut manager = TunnelManager::default();
    if !manager.start_from_config(&config).await {
        manager.stop_all().await;
        return Err(io::Error::other("Failed to start one or more tunnels.").into());
    }
    println!("Foreground mode started. Press Ctrl+C to stop.");
    wait_for_termination_signal().await?;
    manager.stop_all().await;
    Ok(())
}

pub async fn run_config_watcher(config_path: &Path, poll_interval: Duration) -> AppResult<()> {
    let mut manager = TunnelManager::default();
    let mut last_applied_config = None::<AppConfig>;

    loop {
        match AppConfig::load_for_runtime(config_path) {
            Ok(config) => {
                if last_applied_config.as_ref() != Some(&config) {
                    let applied = manager.reconcile(config.enabled_tunnels()).await;
                    if applied {
                        last_applied_config = Some(config);
                    } else {
                        eprintln!("Will retry the current config on the next poll because some tunnels failed to start.");
                    }
                }
            }
            Err(error) => {
                eprintln!("Failed to read config. Keeping current tunnels unchanged: {}", error);
            }
        }

        tokio::select! {
            result = wait_for_termination_signal() => {
                result?;
                break;
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }

    manager.stop_all().await;
    Ok(())
}

async fn run_tcp_tunnel(
    spec: TunnelConfig,
    listener: TcpListener,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, _) = result?;
                let target = spec.target.clone();
                let tcp_mode = spec.tcp_mode;
                let connection_stop = stop_rx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_tcp_connection(inbound, &target, tcp_mode, connection_stop).await {
                        eprintln!("TCP connection handling failed: {}", error);
                    }
                });
            }
            _ = wait_for_shutdown(&mut stop_rx) => {
                break;
            }
        }
    }

    Ok(())
}

async fn handle_tcp_connection(
    inbound: TcpStream,
    target: &str,
    tcp_mode: TcpMode,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let outbound = tokio::select! {
        result = TcpStream::connect(target) => result?,
        _ = wait_for_shutdown(&mut stop_rx) => return Ok(()),
    };

    configure_tcp_streams(&inbound, &outbound, tcp_mode)?;

    #[cfg(target_os = "linux")]
    {
        match tcp_mode.effective() {
            TcpMode::Throughput => handle_tcp_connection_splice(inbound, outbound, stop_rx).await,
            TcpMode::Latency => handle_tcp_connection_copy(inbound, outbound, stop_rx).await,
            TcpMode::Auto => unreachable!("auto mode should resolve before selecting a runtime path"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = tcp_mode;
        handle_tcp_connection_copy(inbound, outbound, stop_rx).await
    }
}

fn configure_tcp_streams(inbound: &TcpStream, outbound: &TcpStream, tcp_mode: TcpMode) -> io::Result<()> {
    let nodelay = matches!(tcp_mode.effective(), TcpMode::Latency);
    inbound.set_nodelay(nodelay)?;
    outbound.set_nodelay(nodelay)?;
    Ok(())
}

async fn handle_tcp_connection_copy(
    mut inbound: TcpStream,
    mut outbound: TcpStream,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    tokio::select! {
        result = copy_bidirectional_with_sizes(
            &mut inbound,
            &mut outbound,
            TCP_COPY_BUFFER_SIZE,
            TCP_COPY_BUFFER_SIZE,
        ) => {
            result?;
        }
        _ = wait_for_shutdown(&mut stop_rx) => {}
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn handle_tcp_connection_splice(
    inbound: TcpStream,
    outbound: TcpStream,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let inbound = Arc::new(inbound);
    let outbound = Arc::new(outbound);

    tokio::select! {
        result = async {
            let result = tokio::try_join!(
                splice_one_way(inbound.clone(), outbound.clone()),
                splice_one_way(outbound.clone(), inbound.clone()),
            );

            if result.is_err() {
                shutdown_socket(inbound.as_ref(), Shutdown::Both);
                shutdown_socket(outbound.as_ref(), Shutdown::Both);
            }

            result
        } => {
            result?;
        }
        _ = wait_for_shutdown(&mut stop_rx) => {
            shutdown_socket(inbound.as_ref(), Shutdown::Both);
            shutdown_socket(outbound.as_ref(), Shutdown::Both);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn splice_one_way(source: Arc<TcpStream>, destination: Arc<TcpStream>) -> io::Result<()> {
    let pipe = SplicePipe::new()?;

    loop {
        source.readable().await?;
        let copied = match source.try_io(Interest::READABLE, || {
            splice_raw(
                source.as_ref().as_raw_fd(),
                pipe.write.as_raw_fd(),
                SPLICE_CHUNK_SIZE,
            )
        }) {
            Ok(copied) => copied,
            Err(_) => continue,
        };

        if copied == 0 {
            shutdown_socket(destination.as_ref(), Shutdown::Write);
            return Ok(());
        }

        let mut remaining = copied;
        while remaining > 0 {
            destination.writable().await?;
            let moved = match destination.try_io(Interest::WRITABLE, || {
                splice_raw(
                    pipe.read.as_raw_fd(),
                    destination.as_ref().as_raw_fd(),
                    remaining,
                )
            }) {
                Ok(moved) => moved,
                Err(_) => continue,
            };

            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "splice returned zero bytes while forwarding to the destination socket",
                ));
            }

            remaining -= moved;
        }
    }
}

#[cfg(target_os = "linux")]
fn shutdown_socket(stream: &TcpStream, how: Shutdown) {
    let _ = unsafe { libc::shutdown(stream.as_raw_fd(), shutdown_mode(how)) };
}

#[cfg(target_os = "linux")]
fn shutdown_mode(how: Shutdown) -> libc::c_int {
    match how {
        Shutdown::Read => libc::SHUT_RD,
        Shutdown::Write => libc::SHUT_WR,
        Shutdown::Both => libc::SHUT_RDWR,
    }
}

#[cfg(target_os = "linux")]
fn splice_raw(fd_in: RawFd, fd_out: RawFd, length: usize) -> io::Result<usize> {
    loop {
        let result = unsafe {
            libc::splice(
                fd_in,
                std::ptr::null_mut(),
                fd_out,
                std::ptr::null_mut(),
                length,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE | libc::SPLICE_F_NONBLOCK,
            )
        };

        if result >= 0 {
            return Ok(result as usize);
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }

        return Err(error);
    }
}

#[cfg(target_os = "linux")]
struct SplicePipe {
    read: OwnedFd,
    write: OwnedFd,
}

#[cfg(target_os = "linux")]
impl SplicePipe {
    fn new() -> io::Result<Self> {
        let mut fds = [0; 2];
        let flags = libc::O_CLOEXEC | libc::O_NONBLOCK;
        if unsafe { libc::pipe2(fds.as_mut_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let _ = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETPIPE_SZ, SPLICE_PIPE_SIZE) };

        Ok(Self { read, write })
    }
}

async fn run_udp_tunnel(
    spec: TunnelConfig,
    listener: Arc<UdpSocket>,
    mut stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut sessions = HashMap::<std::net::SocketAddr, UdpSession>::new();
    let mut buffer = vec![0_u8; 65_535];
    let mut cleanup_tick = interval(UDP_CLEANUP_INTERVAL);
    cleanup_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    cleanup_tick.tick().await;

    loop {
        tokio::select! {
            result = listener.recv_from(&mut buffer) => {
                let (size, client_addr) = result?;

                if let Entry::Vacant(entry) = sessions.entry(client_addr) {
                    let session = create_udp_session(client_addr, spec.target.clone(), listener.clone(), stop_rx.clone()).await?;
                    entry.insert(session);
                }

                if let Some(session) = sessions.get_mut(&client_addr) {
                    session.last_seen = Instant::now();
                    session.upstream.send(&buffer[..size]).await?;
                }
            }
            _ = cleanup_tick.tick() => {
                retire_stale_udp_sessions(&mut sessions);
            }
            _ = wait_for_shutdown(&mut stop_rx) => {
                break;
            }
        }
    }

    shutdown_udp_sessions(sessions).await;

    Ok(())
}

async fn create_udp_session(
    client_addr: std::net::SocketAddr,
    target: String,
    listener: Arc<UdpSocket>,
    global_stop_rx: watch::Receiver<bool>,
) -> AppResult<UdpSession> {
    let upstream = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    upstream.connect(&target).await?;

    let (session_stop_tx, session_stop_rx) = watch::channel(false);
    let session_upstream = upstream.clone();
    let handle = tokio::spawn(async move {
        if let Err(error) = pump_udp_responses(
            listener,
            session_upstream,
            client_addr,
            global_stop_rx,
            session_stop_rx,
        )
        .await
        {
            eprintln!("UDP session handling failed: {}", error);
        }
    });

    Ok(UdpSession {
        upstream,
        last_seen: Instant::now(),
        stop_tx: session_stop_tx,
        handle,
    })
}

fn retire_stale_udp_sessions(sessions: &mut HashMap<std::net::SocketAddr, UdpSession>) {
    let stale = sessions
        .iter()
        .filter_map(|(addr, session)| {
            if session.last_seen.elapsed() > UDP_SESSION_IDLE_TIMEOUT {
                Some(*addr)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for addr in stale {
        if let Some(session) = sessions.remove(&addr) {
            retire_udp_session(session);
        }
    }
}

fn retire_udp_session(session: UdpSession) {
    let _ = session.stop_tx.send(true);
    tokio::spawn(async move {
        let _ = session.handle.await;
    });
}

async fn shutdown_udp_sessions(sessions: HashMap<std::net::SocketAddr, UdpSession>) {
    let mut handles = Vec::with_capacity(sessions.len());
    for (_, session) in sessions {
        let _ = session.stop_tx.send(true);
        handles.push(session.handle);
    }

    for handle in handles {
        let _ = handle.await;
    }
}

async fn pump_udp_responses(
    listener: Arc<UdpSocket>,
    upstream: Arc<UdpSocket>,
    client_addr: std::net::SocketAddr,
    mut global_stop_rx: watch::Receiver<bool>,
    mut session_stop_rx: watch::Receiver<bool>,
) -> AppResult<()> {
    let mut buffer = vec![0_u8; 65_535];

    loop {
        tokio::select! {
            result = upstream.recv(&mut buffer) => {
                let size = result?;
                listener.send_to(&buffer[..size], client_addr).await?;
            }
            _ = wait_for_shutdown(&mut global_stop_rx) => {
                break;
            }
            _ = wait_for_shutdown(&mut session_stop_rx) => {
                break;
            }
        }
    }

    Ok(())
}

async fn wait_for_shutdown(stop_rx: &mut watch::Receiver<bool>) {
    if *stop_rx.borrow() {
        return;
    }

    while stop_rx.changed().await.is_ok() {
        if *stop_rx.borrow() {
            return;
        }
    }
}

async fn wait_for_termination_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}
