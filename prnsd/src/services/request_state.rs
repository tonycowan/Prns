use std::sync::Arc;
use std::time::Instant;

use personal_rns::identity::IdentityHash;
use personal_rns::rns_remote_management::RemoteTransportStatus;
use personal_rns::runtime::PrnsNodeHandle;

use crate::nnpages::NnPagesCatalog;
use personal_rns::remote_control::{RemoteControlRequestKind, RemoteControlRequestSet};
use personal_rns::runtime::{
    RemoteControlHostCommand, RemoteControlHostCommandError, RemoteControlHostControls,
    RemoteControlHostResponse,
};

use super::interface_controls::DaemonInterfaceControls;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportStatusIdentity {
    pub transport: IdentityHash,
    pub network: Option<IdentityHash>,
    pub probe_responder: Option<personal_rns::wire::DestinationHash>,
}

#[derive(Clone)]
pub struct DaemonRequestState {
    handle: PrnsNodeHandle,
    transport: Option<TransportStatusIdentity>,
    started: Instant,
    nnpages: NnPagesCatalog,
    controls: Arc<DaemonInterfaceControls>,
}

impl DaemonRequestState {
    pub fn new(
        handle: PrnsNodeHandle,
        transport: Option<TransportStatusIdentity>,
        started: Instant,
        nnpages: NnPagesCatalog,
        controls: Arc<DaemonInterfaceControls>,
    ) -> Self {
        Self {
            handle,
            transport,
            started,
            nnpages,
            controls,
        }
    }

    pub(super) fn controls(&self) -> &DaemonInterfaceControls {
        &self.controls
    }

    pub fn handle(&self) -> &PrnsNodeHandle {
        &self.handle
    }

    pub fn nnpages(&self) -> &NnPagesCatalog {
        &self.nnpages
    }

    pub fn transport_status(&self) -> Option<RemoteTransportStatus> {
        self.transport.map(|identity| RemoteTransportStatus {
            transport_identity: identity.transport,
            network_identity: identity.network,
            uptime: self.started.elapsed(),
            probe_responder: identity.probe_responder,
        })
    }
}

/// Requests this daemon both advertises and executes. Node setup keeps a host-handled
/// request only when this set lists it; an empty host set drops inventory, path table,
/// power, and transport even if the service capability list includes them.
pub(crate) fn daemon_remote_control_requests() -> RemoteControlRequestSet {
    let mut requests = RemoteControlRequestSet::empty();
    for kind in [
        RemoteControlRequestKind::AnnounceSelf,
        RemoteControlRequestKind::InventoryInterfaces,
        RemoteControlRequestKind::InventoryInterfacePeers,
        RemoteControlRequestKind::InventoryInterfaceConfig,
        RemoteControlRequestKind::InventoryInterfaceDiscoveryGroups,
        RemoteControlRequestKind::ReplaceInterfaceDiscoveryGroups,
        RemoteControlRequestKind::InventoryPathTable,
        RemoteControlRequestKind::InventoryControllers,
        RemoteControlRequestKind::SetInterfacePower,
        RemoteControlRequestKind::SetInterfaceMode,
        RemoteControlRequestKind::SetInterfaceGroup,
        RemoteControlRequestKind::SetInterfaceLoRaProfile,
        RemoteControlRequestKind::SetNetworkTransport,
        RemoteControlRequestKind::DescribeBuild,
        RemoteControlRequestKind::DescribePower,
        RemoteControlRequestKind::DescribeNetworkTransport,
        RemoteControlRequestKind::DescribeTcpClient,
        RemoteControlRequestKind::SetTcpClient,
        RemoteControlRequestKind::AuthorizeController,
        RemoteControlRequestKind::RevokeController,
    ] {
        let _inserted = requests.insert(kind);
    }
    requests
}

impl RemoteControlHostControls for DaemonRequestState {
    fn supported_requests(&self) -> RemoteControlRequestSet {
        daemon_remote_control_requests()
    }

    async fn execute_remote_control(
        &self,
        command: RemoteControlHostCommand,
    ) -> Result<RemoteControlHostResponse, RemoteControlHostCommandError> {
        super::remote_control_host::execute(self, command).await
    }
}
