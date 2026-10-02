//! Local command and transfer sockets.
//!
//! On Unix the path is a filesystem socket, mode 0600. On Windows the path is a
//! text file that holds `127.0.0.1:<port>`, and the service listens on that
//! loopback port. Callers pass the same path on both.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::time::Duration;

#[cfg(windows)]
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

pub struct LocalListener {
    #[cfg(unix)]
    inner: UnixListener,
    #[cfg(windows)]
    inner: TcpListener,
}

pub struct LocalStream {
    #[cfg(unix)]
    inner: UnixStream,
    #[cfg(windows)]
    inner: TcpStream,
}

impl LocalListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        bind(path)
    }

    pub fn accept(&self) -> io::Result<(LocalStream, ())> {
        let (inner, _) = self.inner.accept()?;
        #[cfg(windows)]
        inner.set_nodelay(true)?;
        Ok((LocalStream { inner }, ()))
    }

    pub fn incoming(&self) -> Incoming<'_> {
        Incoming { listener: self }
    }
}

pub struct Incoming<'a> {
    listener: &'a LocalListener,
}

impl Iterator for Incoming<'_> {
    type Item = io::Result<LocalStream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept().map(|(stream, _)| stream))
    }
}

impl LocalStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        connect(path)
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }
}

impl Read for LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}

impl Write for LocalStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Read for &LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (&self.inner).read(buffer)
    }
}

impl Write for &LocalStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        (&self.inner).write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.inner).flush()
    }
}

#[cfg(unix)]
fn bind(path: &Path) -> io::Result<LocalListener> {
    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(io::Error::new(
                ErrorKind::AddrInUse,
                format!(
                    "object store service is already listening on {}",
                    path.display()
                ),
            ));
        }
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let inner = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(LocalListener { inner })
}

#[cfg(unix)]
fn connect(path: &Path) -> io::Result<LocalStream> {
    Ok(LocalStream {
        inner: UnixStream::connect(path)?,
    })
}

#[cfg(windows)]
fn bind(path: &Path) -> io::Result<LocalListener> {
    if path.exists() {
        match connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    ErrorKind::AddrInUse,
                    format!(
                        "object store service is already listening on {}",
                        path.display()
                    ),
                ));
            }
            Err(error) if error.kind() == ErrorKind::ConnectionRefused => {
                fs::remove_file(path)?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let inner = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = inner.local_addr()?.port();
    let mut file = fs::File::create(path)?;
    writeln!(file, "127.0.0.1:{port}")?;
    file.sync_all()?;
    Ok(LocalListener { inner })
}

#[cfg(windows)]
fn connect(path: &Path) -> io::Result<LocalStream> {
    let text = fs::read_to_string(path)?;
    let addr = parse_loopback(text.trim())?;
    let inner = TcpStream::connect(addr)?;
    inner.set_nodelay(true)?;
    Ok(LocalStream { inner })
}

#[cfg(windows)]
fn parse_loopback(text: &str) -> io::Result<SocketAddr> {
    let addr: SocketAddr = text.parse().map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidData,
            "object service address file is invalid",
        )
    })?;
    if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "object service address file is invalid",
        ));
    }
    Ok(addr)
}
