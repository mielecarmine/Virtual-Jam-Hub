//! UDP deployment check: cargo run -p jam-server --example probe -- IP:7777

use std::error::Error;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use jam_protocol::{ControlMessage, MAX_PACKET_SIZE, NICKNAME_CAPACITY, SAMPLE_RATE};

fn exchange(
    socket: &UdpSocket,
    request: &ControlMessage,
    accept: impl Fn(&ControlMessage) -> bool,
) -> Result<ControlMessage, Box<dyn Error>> {
    let mut buffer = [0; MAX_PACKET_SIZE];
    for _ in 0..6 {
        let length = request.to_bytes(&mut buffer)?;
        socket.send(&buffer[..length])?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match socket.recv(&mut buffer) {
                Ok(length) => {
                    let response = ControlMessage::from_bytes(&buffer[..length])?;
                    if accept(&response) {
                        return Ok(response);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "no expected UDP response in 3s").into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let address: SocketAddr = arguments.next().ok_or("usage: probe IP:7777")?.parse()?;
    if arguments.next().is_some() {
        return Err("usage: probe IP:7777".into());
    }
    let socket = UdpSocket::bind(if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?;
    socket.connect(address)?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut nickname = [0; NICKNAME_CAPACITY];
    nickname[..9].copy_from_slice(b"jam-probe");
    let response = exchange(
        &socket,
        &ControlMessage::JoinRequest {
            nickname,
            nickname_len: 9,
        },
        |message| matches!(message, ControlMessage::JoinAccepted { sample_rate, .. } if *sample_rate == SAMPLE_RATE),
    )?;
    let ControlMessage::JoinAccepted {
        assigned_peer_id, ..
    } = response
    else {
        unreachable!();
    };
    println!("JoinAccepted from {address}: peer {assigned_peer_id}, {SAMPLE_RATE} Hz");
    let result = exchange(
        &socket,
        &ControlMessage::Ping {
            client_timestamp_us: 42,
        },
        |message| {
            matches!(
                message,
                ControlMessage::Pong {
                    client_timestamp_us: 42,
                    ..
                }
            )
        },
    );
    let mut buffer = [0; MAX_PACKET_SIZE];
    let length = ControlMessage::Leave {
        peer_id: assigned_peer_id,
    }
    .to_bytes(&mut buffer)?;
    socket.send(&buffer[..length])?;
    println!("{:?} from {address}", result?);
    Ok(())
}
