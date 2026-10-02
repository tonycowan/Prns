use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use personal_rns::remote_control::{RemoteControlTcpClientHost, RemoteControlTcpClientTarget};

use super::super::*;

pub(in crate::s3) static TCP_RETARGET: TcpRetarget = TcpRetarget::new();
pub(in crate::s3) static TCP_CONFIG: Mutex<
    CriticalSectionRawMutex,
    Option<RemoteControlTcpClientTarget>,
> = Mutex::new(None);

const TCP_CHANNEL_TAG: &[u8] = b"hopspot-tcp";

pub(in crate::s3) fn tcp_interface_id() -> InterfaceId {
    TcpClient::interface_id(TCP_CHANNEL_TAG)
}

pub(in crate::s3) fn build_tcp(
    stack: Stack<'static>,
) -> (
    TcpClient<'static>,
    &'static EmbassyInterfaceStatus,
    InterfaceId,
) {
    let id = tcp_interface_id();
    let status: &'static EmbassyInterfaceStatus = mk_static!(
        EmbassyInterfaceStatus,
        EmbassyInterfaceStatus::new_accounted(id, ConnectionState::Disconnected)
    );
    let rx_buffer: &'static mut [u8] =
        crate::storage::allocate_psram_slice(TCP_SOCKET_BUFFER_BYTES, 0u8);
    let tx_buffer: &'static mut [u8] =
        crate::storage::allocate_psram_slice(TCP_SOCKET_BUFFER_BYTES, 0u8);
    let tcp = TcpClient::new(TcpClientInput {
        stack,
        target: TcpClientTarget::endpoint(IpEndpoint::new(
            core::net::Ipv4Addr::UNSPECIFIED.into(),
            1,
        )),
        retarget: Some(&TCP_RETARGET),
        channel_tag: TCP_CHANNEL_TAG,
        bitrate: TCP_BITRATE_BPS,
        reconnect_policy: ReconnectPolicy::STANDARD,
        socket_buffers: TcpSocketBuffers {
            rx: rx_buffer,
            tx: tx_buffer,
        },
        status,
    });
    (tcp, status, id)
}

pub(in crate::s3) fn embassy_tcp_target(
    target: &RemoteControlTcpClientTarget,
) -> Option<TcpClientTarget> {
    match target.host() {
        RemoteControlTcpClientHost::Ipv4(address) => Some(TcpClientTarget::endpoint(
            IpEndpoint::new(core::net::Ipv4Addr::from(address).into(), target.port()),
        )),
        RemoteControlTcpClientHost::Hostname { .. } => {
            let hostname =
                heapless::String::<TCP_DNS_HOSTNAME_MAX_BYTES>::try_from(target.host().as_str())
                    .ok()?;
            Some(TcpClientTarget::dns(hostname, target.port()))
        }
    }
}

pub(in crate::s3) fn hopspot_tcp_config(
    target: &RemoteControlTcpClientTarget,
) -> Option<HopspotTcpClientConfig> {
    let host = match target.host() {
        RemoteControlTcpClientHost::Ipv4(address) => {
            HopspotTcpClientHost::Ipv4(core::net::Ipv4Addr::from(address))
        }
        RemoteControlTcpClientHost::Hostname { .. } => {
            HopspotTcpClientHost::Hostname(target.host().as_str().to_string())
        }
    };
    Some(HopspotTcpClientConfig {
        host,
        port: target.port(),
    })
}

pub(in crate::s3) fn remote_tcp_target(
    config: &HopspotTcpClientConfig,
) -> Option<RemoteControlTcpClientTarget> {
    let text = match &config.host {
        HopspotTcpClientHost::Ipv4(address) => alloc::format!("{address}:{}", config.port),
        HopspotTcpClientHost::Hostname(hostname) => alloc::format!("{hostname}:{}", config.port),
    };
    RemoteControlTcpClientTarget::parse(&text).ok()
}

pub(in crate::s3) async fn install_tcp_target(target: Option<RemoteControlTcpClientTarget>) {
    *TCP_CONFIG.lock().await = target;
    let embassy = target.as_ref().and_then(embassy_tcp_target);
    TCP_RETARGET.replace(embassy).await;
}
