//! Native HTTP server for unattended EBC battery tests.

use ebc_battery_tester::server::{self, ServerConfig};

#[tokio::main]
async fn main() {
    env_logger::init();
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        if let Err(error) = healthcheck() {
            log::error!("healthcheck failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let result = ServerConfig::from_env()
        .map(|config| (config.http_addr, config))
        .map_err(|error| format!("configuration error: {error}"));
    let (address, config) = match result {
        Ok(value) => value,
        Err(error) => {
            log::error!("{error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = server::run(config).await {
        log::error!("server at {address} stopped: {error}");
        std::process::exit(1);
    }
}

fn healthcheck() -> Result<(), String> {
    use std::io::{Read as _, Write as _};
    use std::net::{IpAddr, SocketAddr, TcpStream};
    use std::time::Duration;

    let configured = std::env::var("EBC_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let mut address: SocketAddr = configured
        .parse()
        .map_err(|error| format!("invalid EBC_HTTP_ADDR: {error}"))?;
    if address.ip().is_unspecified() {
        address.set_ip(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("cannot connect to {address}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /api/status HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut response = [0_u8; 12];
    stream
        .read_exact(&mut response)
        .map_err(|error| error.to_string())?;
    if response == *b"HTTP/1.0 200" || response == *b"HTTP/1.1 200" {
        Ok(())
    } else {
        Err("server returned a non-200 response".to_owned())
    }
}
