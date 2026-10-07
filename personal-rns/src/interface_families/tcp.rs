#[cfg(feature = "tokio-host")]
pub use prns_interfaces_tokio::tcp::{
    tune, AddressFamilyPreference, ReconnectLimit, TcpClientControl, TcpClientInterface,
    TcpConnectionSettings, TcpServer, TcpServerConnection, TcpServerStatus, TcpTunnelMode,
    CONNECT_TIMEOUT,
};

#[cfg(all(feature = "embassy-host", not(feature = "tokio-host")))]
pub use prns_interfaces_embassy::tcp::{
    TcpClient, TcpClientExitCause, TcpClientInput, TcpClientTarget, TcpRetarget, TcpSocketBuffers,
    CONNECT_TIMEOUT, KEEP_ALIVE, SOCKET_TIMEOUT, TCP_DNS_HOSTNAME_MAX_BYTES,
};
