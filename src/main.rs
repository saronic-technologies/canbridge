use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use socketcan::{CanFdSocket, Socket};
use std::net::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{self, Sender, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// CAN-TCP Bridge - Forward CAN frames over TCP bidirectionally
#[derive(Parser, Debug)]
#[command(name = "canbridge", version, about)]
struct Args {
    /// Mode of operation
    #[arg(short, long)]
    mode: Mode,

    /// Address to listen on or connect to (host:port)
    #[arg(short, long)]
    addr: String,

    /// CAN interface name (e.g., can0, vcan0)
    #[arg(short, long)]
    iface: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    Listen,
    Connect,
}

// Import wire protocol functions from lib
use canbridge::{
    can_to_wire, frame_hash, recv_wire, send_wire, wire_to_can,
};

// SocketCAN constants
const SOL_CAN_RAW: libc::c_int = 101;
const CAN_RAW_FD_FRAMES: libc::c_int = 5;

/// Enable CAN FD frames on the socket
fn enable_canfd(socket: &CanFdSocket) -> Result<()> {
    let enable: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            SOL_CAN_RAW,
            CAN_RAW_FD_FRAMES,
            &enable as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };

    if ret < 0 {
        return Err(anyhow!(
            "Failed to enable CAN FD: {}",
            std::io::Error::last_os_error()
        ));
    }

    Ok(())
}

/// Run the CAN → TCP forwarding loop with deduplication
///
/// This function reads frames from the CAN bus and forwards them over TCP.
/// It maintains a local HashSet of recently sent frame hashes (received via channel)
/// to filter out frames that we transmitted ourselves (which appear due to kernel loopback).
fn can_to_tcp_loop(
    socket: CanFdSocket,
    mut stream: TcpStream,
    iface: &str,
    sent_frames_rx: Receiver<u64>,
) -> Result<()> {
    info!(interface = %iface, "[CAN→TCP] Starting forwarding loop");

    // Local map of frames we've recently sent with their timestamps
    let mut sent_frames: HashMap<u64, Instant> = HashMap::new();
    // Threshold for removing old entries (100ms should be more than enough for loopback)
    const CLEANUP_THRESHOLD: Duration = Duration::from_millis(100);

    loop {
        let frame = socket
            .read_frame()
            .context("Failed to read frame from CAN")?;

        let wire = can_to_wire(&frame);
        let hash = frame_hash(wire.can_id, &wire.data);

        // Drain any pending hashes from the channel AFTER receiving a frame
        // This ensures we catch any hashes that were sent while we were blocked on read_frame
        let now = Instant::now();
        while let Ok(hash) = sent_frames_rx.try_recv() {
            sent_frames.insert(hash, now);
        }

        // Clean up old entries that are past the threshold
        // This prevents unbounded growth while avoiding race conditions
        let old_count = sent_frames.len();
        sent_frames.retain(|_, timestamp| now.duration_since(*timestamp) < CLEANUP_THRESHOLD);
        if old_count > sent_frames.len() {
            debug!(
                removed = old_count - sent_frames.len(),
                remaining = sent_frames.len(),
                "[CAN→TCP] Cleaned up old frame hashes"
            );
        }

        // Check if this is a frame we recently sent (looped back)
        if sent_frames.remove(&hash).is_some() {
            // This is a looped-back frame we sent, skip it
            debug!(interface = %iface, "[CAN→TCP] Skipping looped-back frame");
            continue;
        }

        debug!(interface = %iface, ?frame, "[CAN→TCP] Received frame from CAN");

        if let Err(e) = send_wire(&mut stream, &wire) {
            error!(error = %e, "[CAN→TCP] Failed to send frame over TCP");
            return Err(e);
        }
        debug!(interface = %iface, "[CAN→TCP] Sent frame over TCP");
    }
}

/// Run the TCP → CAN forwarding loop with deduplication tracking
///
/// This function receives frames from TCP and sends them to the CAN bus.
/// After successfully sending each frame, it notifies the reader thread via channel
/// so the reader can filter out the loopback when it appears.
fn tcp_to_can_loop(
    socket: CanFdSocket,
    mut stream: TcpStream,
    iface: &str,
    sent_frames_tx: Sender<u64>,
) -> Result<()> {
    info!(interface = %iface, "[TCP→CAN] Starting forwarding loop");

    loop {
        let wire = match recv_wire(&mut stream) {
            Ok(w) => w,
            Err(e) => {
                error!(error = %e, "[TCP→CAN] Failed to receive frame from TCP");
                return Err(e);
            }
        };

        debug!(
            interface = %iface,
            can_id = %format_args!("{:#x}", wire.can_id),
            data = ?wire.data,
            "[TCP→CAN] Received frame from TCP"
        );

        let frame = match wire_to_can(&wire) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "[TCP→CAN] Failed to convert wire frame to CAN");
                continue;
            }
        };

        // Notify the reader thread BEFORE sending to avoid race condition
        // where the loopback arrives before the hash is in the reader's set
        let hash = frame_hash(wire.can_id, &wire.data);
        // Ignore send errors - if the receiver is gone, we're shutting down anyway
        let _ = sent_frames_tx.send(hash);

        if let Err(e) = socket.write_frame(&frame) {
            error!(error = %e, "[TCP→CAN] Failed to write frame to CAN");
            // Note: We already sent the hash, but the reader will eventually
            // clean it up since no matching frame will arrive
            return Err(e.into());
        }
        debug!(interface = %iface, "[TCP→CAN] Wrote frame to CAN");
    }
}

/// Handle a single TCP connection for bidirectional bridging
fn handle_connection(stream: TcpStream, iface: &str) -> Result<()> {
    info!(interface = %iface, "Connection established, starting bridge");

    // Open two CAN sockets (one for each direction)
    let can_read = CanFdSocket::open(iface).context("Failed to open CAN socket for reading")?;
    let can_write = CanFdSocket::open(iface).context("Failed to open CAN socket for writing")?;

    // Enable CAN FD on both sockets
    if let Err(e) = enable_canfd(&can_read) {
        warn!(error = %e, "Could not enable CAN FD for read socket");
    }
    if let Err(e) = enable_canfd(&can_write) {
        warn!(error = %e, "Could not enable CAN FD for write socket");
    }

    // Create a channel for the writer to notify the reader about sent frames
    // This allows the reader to filter out looped-back frames
    let (sent_frames_tx, sent_frames_rx) = mpsc::channel();

    // Clone TCP stream for bidirectional communication
    let stream_read = stream.try_clone().context("Failed to clone TCP stream")?;
    let stream_write = stream;

    let iface_clone = iface.to_string();

    // Spawn CAN → TCP thread (reads from CAN, writes to TCP)
    let can_to_tcp_handle = thread::spawn(move || {
        if let Err(e) = can_to_tcp_loop(can_read, stream_write, &iface_clone, sent_frames_rx) {
            error!(error = %e, "[CAN→TCP] Thread exited with error");
        }
    });

    // Run TCP → CAN in main thread (reads from TCP, writes to CAN)
    let iface_clone = iface.to_string();
    if let Err(e) = tcp_to_can_loop(can_write, stream_read, &iface_clone, sent_frames_tx) {
        error!(error = %e, "[TCP→CAN] Loop exited with error");
    }

    // Wait for the other thread
    let _ = can_to_tcp_handle.join();

    info!(interface = %iface, "Connection closed");
    Ok(())
}

/// Handle a connection with pre-opened CAN read socket (for server mode)
fn handle_connection_with_can_socket(
    stream: TcpStream,
    can_read: CanFdSocket,
    iface: &str,
) -> Result<()> {
    info!(interface = %iface, "Connection established, starting bridge");

    // Open write socket for TCP→CAN direction
    let can_write = CanFdSocket::open(iface).context("Failed to open CAN socket for writing")?;

    // Enable CAN FD on write socket
    if let Err(e) = enable_canfd(&can_write) {
        warn!(error = %e, "Could not enable CAN FD for write socket");
    }

    // Create a channel for the writer to notify the reader about sent frames
    let (sent_frames_tx, sent_frames_rx) = mpsc::channel();

    // Clone TCP stream for bidirectional communication
    let stream_read = stream.try_clone().context("Failed to clone TCP stream")?;
    let stream_write = stream;

    let iface_clone = iface.to_string();

    // Spawn CAN → TCP thread
    let can_to_tcp_handle = thread::spawn(move || {
        if let Err(e) = can_to_tcp_loop(can_read, stream_write, &iface_clone, sent_frames_rx) {
            error!(error = %e, "[CAN→TCP] Thread exited with error");
        }
    });

    // Run TCP → CAN in main thread
    let iface_clone = iface.to_string();
    if let Err(e) = tcp_to_can_loop(can_write, stream_read, &iface_clone, sent_frames_tx) {
        error!(error = %e, "[TCP→CAN] Loop exited with error");
    }

    // Wait for the other thread
    let _ = can_to_tcp_handle.join();

    info!(interface = %iface, "Connection closed");
    Ok(())
}

/// Run in server (listen) mode
fn run_server(addr: &str, iface: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).context("Failed to bind to address")?;
    info!(address = %addr, interface = %iface, "Server listening");

    loop {
        // Open CAN read socket BEFORE accepting connection so frames are buffered
        let can_read =
            CanFdSocket::open(iface).context("Failed to open CAN socket for reading")?;

        // Enable CAN FD on read socket
        if let Err(e) = enable_canfd(&can_read) {
            warn!(error = %e, "Could not enable CAN FD for read socket");
        }

        debug!("CAN socket ready, waiting for TCP connection");

        match listener.accept() {
            Ok((stream, peer_addr)) => {
                info!(peer = %peer_addr, "Accepted connection");
                if let Err(e) = handle_connection_with_can_socket(stream, can_read, iface) {
                    error!(error = %e, "Connection error");
                }
                debug!("Waiting for next connection");
            }
            Err(e) => {
                error!(error = %e, "Failed to accept connection");
            }
        }
    }
}

/// Run in client (connect) mode
fn run_client(addr: &str, iface: &str) -> Result<()> {
    loop {
        info!(address = %addr, interface = %iface, "Connecting to server");

        match TcpStream::connect(addr) {
            Ok(stream) => {
                if let Err(e) = handle_connection(stream, iface) {
                    error!(error = %e, "Connection error");
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to connect to server");
            }
        }

        // Wait before reconnecting
        info!("Reconnecting in 1 second");
        thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn main() -> Result<()> {
    // Initialize tracing with environment filter
    // Set RUST_LOG=debug for debug output, RUST_LOG=info for normal output
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()))
        .init();

    let args = Args::parse();

    match args.mode {
        Mode::Listen => run_server(&args.addr, &args.iface),
        Mode::Connect => run_client(&args.addr, &args.iface),
    }
}
