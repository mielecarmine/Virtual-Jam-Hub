//! Headless Virtual Jam server entry point.

fn main() -> std::io::Result<()> {
    let bind = std::env::args().nth(1);
    let mut server = jam_server::Server::bind(bind.as_deref().unwrap_or("0.0.0.0:7777"))?;
    loop {
        server.poll()?;
    }
}
