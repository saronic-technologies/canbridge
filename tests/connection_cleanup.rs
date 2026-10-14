//! Privileged vcan regression, run by nix/tests/connection-cleanup.nix in an isolated VM.
use canbridge::{can_to_wire, recv_wire, send_wire, wire_to_can, WireFrameV1};
use socketcan::{CanFdSocket, Socket};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const ADDR: &str = "127.0.0.1:4242";
const IFACE: &str = "vcan0";
const UNIT: &str = "canbridge-cleanup.service";

fn command(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(output.status.success(), "{program} {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn wait_for(description: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        sleep(Duration::from_millis(50));
    }
}

fn pid() -> String {
    command(
        "systemctl",
        &["show", UNIT, "--property=MainPID", "--value"],
    )
    .trim()
    .to_owned()
}

fn journal(pid: &str) -> String {
    command(
        "journalctl",
        &[
            "--no-pager",
            "-o",
            "cat",
            &format!("_PID={pid}"),
            "-u",
            UNIT,
        ],
    )
}

fn connect(mode: &str, listener: Option<&TcpListener>) -> TcpStream {
    let mut stream = None;
    wait_for("TCP connection", || {
        stream = if mode == "listen" {
            TcpStream::connect(ADDR).ok()
        } else {
            listener.unwrap().accept().ok().map(|(stream, _)| stream)
        };
        stream.is_some()
    });
    let stream = stream.unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
    stream
}

fn listener(mode: &str) -> Option<TcpListener> {
    (mode == "connect").then(|| {
        let listener = TcpListener::bind(ADDR).unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    })
}

fn verify_forwarding(stream: &mut TcpStream, process: &str, sequence: u8) {
    wait_for("both forwarding loops", || {
        let log = journal(process);
        log.contains("[CAN→TCP] Starting forwarding loop")
            && log.contains("[TCP→CAN] Starting forwarding loop")
    });
    let can = CanFdSocket::open(IFACE).unwrap();
    can.set_read_timeout(Some(TIMEOUT)).unwrap();
    let from_can = WireFrameV1::new(0x123, &[sequence, 0xCA], 0);
    can.write_frame(&wire_to_can(&from_can).unwrap()).unwrap();
    assert_eq!(recv_wire(stream).unwrap(), from_can);
    let from_tcp = WireFrameV1::new(0x456, &[sequence, 0xDC], 0);
    send_wire(stream, &from_tcp).unwrap();
    assert_eq!(can_to_wire(&can.read_frame().unwrap()), from_tcp);

    // No bridge echo or duplicate may be left pending when the idle test starts.
    stream
        .set_read_timeout(Some(Duration::from_millis(750)))
        .unwrap();
    let mut byte = [0];
    let error = stream
        .read(&mut byte)
        .expect_err("unexpected echo or closed healthy session");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    println!("{process}: bidirectional forwarding and no echo verified");
}

fn regression(mode: &str) {
    let endpoint = listener(mode);
    command("systemctl", &["start", UNIT]);
    let mut stream = connect(mode, endpoint.as_ref());
    let original_pid = pid();
    assert_ne!(original_pid, "0");
    verify_forwarding(&mut stream, &original_pid, 1);

    // From here until EOF is proven, never write TCP, raise CAN, or restart the service.
    command("ip", &["link", "set", IFACE, "down"]);
    let started = Instant::now();
    let mut byte = [0];
    let result = stream.read(&mut byte);
    assert_eq!(
        pid(),
        original_pid,
        "process restart must not prove local cleanup"
    );
    let flags = std::fs::read_to_string(format!("/sys/class/net/{IFACE}/flags")).unwrap();
    assert_eq!(
        u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).unwrap() & 1,
        0
    );
    let log = journal(&original_pid);
    assert!(log.contains("[CAN→TCP] Thread exited with error"), "{log}");
    assert!(log.contains("Failed to read frame from CAN"), "{log}");
    assert!(
        matches!(result, Ok(0)),
        "{mode}: idle session did not close within 30s while CAN stayed down: {result:?}"
    );
    println!(
        "{mode}: idle session closed in {:?} with CAN down and PID unchanged",
        started.elapsed()
    );
    wait_for("connection handler cleanup", || {
        journal(&original_pid).contains("Connection closed")
    });
    if mode == "connect" {
        assert!(journal(&original_pid).contains("Reconnecting in 1 second"));
    }
    drop(stream);
    // Discard pending old-process connections before restoration. Rebind only after replacement.
    drop(endpoint);

    command("ip", &["link", "set", IFACE, "up"]);
    wait_for("link monitor replacement process", || {
        let current = pid();
        current != "0" && current != original_pid
    });
    assert!(journal(&original_pid).contains("came back up"));
    let replacement_pid = pid();
    let endpoint = listener(mode);
    let mut stream = connect(mode, endpoint.as_ref());
    verify_forwarding(&mut stream, &replacement_pid, 2);
    assert_eq!(pid(), replacement_pid);
    println!("{mode}: recovered with replacement PID {replacement_pid}");
}

#[test]
#[ignore = "requires the isolated NixOS VM with vcan and systemd"]
fn listen_idle_can_failure() {
    regression("listen");
}

#[test]
#[ignore = "requires the isolated NixOS VM with vcan and systemd"]
fn connect_idle_can_failure() {
    regression("connect");
}
