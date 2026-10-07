use personal_rns::interfaces::InterfaceSnapshot;
use personal_rns::remote_control::{
    PowerSnapshot, RemoteControlBuildVersion, RemoteControlDiscoveryGroupsReplaceOutcome,
    RemoteControlGroupOutcome, RemoteControlLoRaOutcome,
};
use personal_rns::runtime::{
    RemoteControlHostCommand, RemoteControlHostCommandError, RemoteControlHostResponse,
};

use super::remote_control_inventory::{
    remote_control_interface_config, remote_control_interface_peers_from_snapshots,
    remote_control_inventory_from_snapshots,
};
use super::request_state::DaemonRequestState;

pub(super) fn daemon_build_version() -> RemoteControlBuildVersion {
    RemoteControlBuildVersion::from_text(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(RemoteControlBuildVersion::empty)
}

pub(super) async fn execute(
    state: &DaemonRequestState,
    command: RemoteControlHostCommand,
) -> Result<RemoteControlHostResponse, RemoteControlHostCommandError> {
    let entries = state.handle().interface_inventory();
    let snapshots: Vec<InterfaceSnapshot> = entries.iter().map(|entry| entry.snapshot).collect();
    match command {
        RemoteControlHostCommand::InventoryInterfaces { page } => {
            Ok(RemoteControlHostResponse::InventoryInterfaces(
                remote_control_inventory_from_snapshots(&snapshots, page)
                    .map_err(|_| RemoteControlHostCommandError::ApplyFailed)?,
            ))
        }
        RemoteControlHostCommand::InventoryInterfacePeers { id, page } => {
            Ok(RemoteControlHostResponse::InventoryInterfacePeers(
                remote_control_interface_peers_from_snapshots(&snapshots, id, page)
                    .map_err(|_| RemoteControlHostCommandError::ApplyFailed)?,
            ))
        }
        RemoteControlHostCommand::InventoryInterfaceConfig { id } => {
            let group = state.controls().singleton_group(id);
            Ok(RemoteControlHostResponse::InventoryInterfaceConfig(
                remote_control_interface_config(&entries, id, group.as_deref())
                    .map_err(|_| RemoteControlHostCommandError::ApplyFailed)?,
            ))
        }
        RemoteControlHostCommand::SetInterfacePower { id, power } => Ok(
            RemoteControlHostResponse::SetInterfacePower(state.controls().set_power(id, power)),
        ),
        RemoteControlHostCommand::SetInterfaceMode { id, mode } => {
            Ok(RemoteControlHostResponse::SetInterfaceMode(
                state.handle().set_interface_mode(id, mode),
            ))
        }
        RemoteControlHostCommand::SetInterfaceGroup { id, group } => {
            let groups = personal_rns::interfaces::DiscoveryGroupSet::from_singleton(group);
            let outcome = state
                .controls()
                .replace_discovery_groups(id, groups)
                .await?;
            let outcome = match outcome {
                RemoteControlDiscoveryGroupsReplaceOutcome::Applied
                | RemoteControlDiscoveryGroupsReplaceOutcome::Unchanged => {
                    RemoteControlGroupOutcome::Applied
                }
                RemoteControlDiscoveryGroupsReplaceOutcome::UnknownInterface => {
                    RemoteControlGroupOutcome::UnknownInterface
                }
                RemoteControlDiscoveryGroupsReplaceOutcome::Unsupported => {
                    return Err(RemoteControlHostCommandError::Unsupported);
                }
            };
            Ok(RemoteControlHostResponse::SetInterfaceGroup(outcome))
        }
        RemoteControlHostCommand::InventoryInterfaceDiscoveryGroups { id } => Ok(
            RemoteControlHostResponse::InventoryInterfaceDiscoveryGroups(
                state.controls().discovery_groups(id)?,
            ),
        ),
        RemoteControlHostCommand::ReplaceInterfaceDiscoveryGroups { id, groups } => {
            Ok(RemoteControlHostResponse::ReplaceInterfaceDiscoveryGroups(
                state
                    .controls()
                    .replace_discovery_groups(id, groups.into_groups())
                    .await?,
            ))
        }
        RemoteControlHostCommand::SetInterfaceLoRaProfile { .. } => {
            Ok(RemoteControlHostResponse::SetInterfaceLoRaProfile(
                RemoteControlLoRaOutcome::UnknownInterface,
            ))
        }
        RemoteControlHostCommand::DescribeBuild => Ok(RemoteControlHostResponse::DescribeBuild(
            daemon_build_version(),
        )),
        RemoteControlHostCommand::DescribePower => Ok(RemoteControlHostResponse::DescribePower(
            PowerSnapshot::UNKNOWN,
        )),
        RemoteControlHostCommand::DescribeNetworkTransport => {
            let transport = state
                .handle()
                .describe_network_transport()
                .await
                .ok_or(RemoteControlHostCommandError::ApplyFailed)?;
            Ok(RemoteControlHostResponse::DescribeNetworkTransport(
                transport,
            ))
        }
        RemoteControlHostCommand::SetNetworkTransport { transport } => {
            Ok(RemoteControlHostResponse::SetNetworkTransport(
                state.handle().set_network_transport(transport),
            ))
        }
        RemoteControlHostCommand::DescribeTcpClient => Ok(
            RemoteControlHostResponse::DescribeTcpClient(state.controls().describe_tcp_client()?),
        ),
        RemoteControlHostCommand::SetTcpClient { config } => Ok(
            RemoteControlHostResponse::SetTcpClient(state.controls().set_tcp_client(config)?),
        ),
        // A daemon has no display, GNSS receiver, ESP radio mode, or Wi-Fi credential store.
        // Those Hopspot commands stay unsupported. LoRa tuning has no radio controller here.
        _ => Err(RemoteControlHostCommandError::Unsupported),
    }
}
