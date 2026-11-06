use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

/// Builds an origin and context path using the same explicit port format as the Java utility.
pub fn get_origin(scheme: &str, server_name: &str, server_port: u16, context_path: &str) -> String {
    format!("{scheme}://{server_name}:{server_port}{context_path}")
}

/// Returns whether a TCP listener can bind the requested port on all IPv4 interfaces.
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

/// Finds the first available port from `start_port` through the end of the TCP port range.
pub fn find_available_port(start_port: u16) -> io::Result<u16> {
    (start_port..=u16::MAX)
        .find(|port| is_port_available(*port))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "No available port in the requested range",
            )
        })
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    use super::{find_available_port, get_origin, is_port_available};

    #[test]
    fn builds_origin_with_explicit_port_and_context_path() {
        assert_eq!(
            get_origin("https", "example.com", 443, "/app"),
            "https://example.com:443/app"
        );
        assert_eq!(
            get_origin("http", "localhost", 8080, ""),
            "http://localhost:8080"
        );
    }

    #[test]
    fn detects_bound_port_and_finds_an_available_one() {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_port_available(port));
        drop(listener);
        assert!(is_port_available(port));
        assert!(find_available_port(port).is_ok());
    }
}
