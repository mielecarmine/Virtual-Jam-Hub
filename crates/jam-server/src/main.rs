//! Headless Virtual Jam server entry point.

fn main() -> std::io::Result<()> {
    let mut server = jam_server::Server::bind("0.0.0.0:7777")?;
    loop {
        server.poll()?;
    }
}
