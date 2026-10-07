use std::sync::{Arc, Mutex};

use personal_rns::bluetooth_auto::BluetoothAutoStatus;
use personal_rns::interfaces::{
    ConnectionState, DiscoveryGroupApplyOutcome, DiscoveryGroupSet, InterfaceId, InterfaceKind,
    InterfaceStatus,
};
use personal_rns::manifold::tokio::TokioInterfaceStatus;
use personal_rns::remote_control::{
    RemoteControlDiscoveryGroups, RemoteControlDiscoveryGroupsInventoryOutcome,
    RemoteControlDiscoveryGroupsReplaceOutcome, RemoteControlInterfacePower,
    RemoteControlPowerOutcome, RemoteControlTcpClientConfig, RemoteControlTcpClientOutcome,
    RemoteControlTcpClientStatus, RemoteControlTcpClientTarget,
};
use personal_rns::runtime::RemoteControlHostCommandError;
use personal_rns::tcp::TcpClientControl;
use personal_rns::wifi_auto::AutoWifiStatus;
use personal_rns::{InterfaceControlHook, RegisteredInterfaceControl};

struct WifiPower {
    ids: Vec<InterfaceId>,
    status: AutoWifiStatus,
}

struct UsbPower {
    ids: Vec<InterfaceId>,
    status: TokioInterfaceStatus,
}

#[derive(Default)]
pub(crate) struct DaemonInterfaceControls {
    wifi: Mutex<Vec<WifiPower>>,
    bluetooth: Mutex<Vec<BluetoothAutoStatus>>,
    tcp: Mutex<Vec<TcpClientControl>>,
    usb: Mutex<Vec<UsbPower>>,
}

impl DaemonInterfaceControls {
    pub(crate) fn hook(self: &Arc<Self>) -> InterfaceControlHook {
        let controls = Arc::clone(self);
        Arc::new(move |control| controls.register(control))
    }

    fn register(&self, control: RegisteredInterfaceControl) {
        match control {
            RegisteredInterfaceControl::AutoWifi { id, status } => self.store_wifi(id, status),
            RegisteredInterfaceControl::BluetoothAuto(status) => self.store_bluetooth(status),
            RegisteredInterfaceControl::TcpClient(control) => self.store_tcp(control),
            RegisteredInterfaceControl::UsbAuto { id, status } => self.store_usb(id, status),
        }
    }

    fn store_wifi(&self, id: InterfaceId, status: AutoWifiStatus) {
        let Ok(mut statuses) = self.wifi.lock() else {
            return;
        };
        let ids = power_ids(id, status.id());
        if let Some(existing) = statuses
            .iter_mut()
            .find(|current| current.ids.iter().any(|stored| ids.contains(stored)))
        {
            existing.ids = ids;
            existing.status = status;
        } else {
            statuses.push(WifiPower { ids, status });
        }
    }

    fn store_bluetooth(&self, status: BluetoothAutoStatus) {
        let Ok(mut statuses) = self.bluetooth.lock() else {
            return;
        };
        let id = status.id();
        if let Some(existing) = statuses.iter_mut().find(|current| current.id() == id) {
            *existing = status;
        } else {
            statuses.push(status);
        }
    }

    pub(super) fn set_power(
        &self,
        id: InterfaceId,
        power: RemoteControlInterfacePower,
    ) -> RemoteControlPowerOutcome {
        let desired = power == RemoteControlInterfacePower::On;
        match self.wifi(id) {
            Err(()) => return RemoteControlPowerOutcome::Failed,
            Ok(Some(status)) => return apply_wifi_power(&status, desired),
            Ok(None) => {}
        }
        match self.bluetooth(id) {
            Err(()) => return RemoteControlPowerOutcome::Failed,
            Ok(Some(status)) => {
                return apply_power(
                    status.is_enabled(),
                    desired,
                    || status.enable(),
                    || status.disable(),
                );
            }
            Ok(None) => {}
        }
        match self.tcp_status(id) {
            Err(()) => return RemoteControlPowerOutcome::Failed,
            Ok(Some(status)) => {
                return apply_power(
                    status.is_enabled(),
                    desired,
                    || status.enable(),
                    || status.disable(),
                );
            }
            Ok(None) => {}
        }
        match self.usb(id) {
            Err(()) => RemoteControlPowerOutcome::Failed,
            Ok(Some(status)) => apply_power(
                status.is_enabled(),
                desired,
                || status.enable(),
                || status.disable(),
            ),
            Ok(None) => RemoteControlPowerOutcome::UnknownInterface,
        }
    }

    pub(super) fn discovery_groups(
        &self,
        id: InterfaceId,
    ) -> Result<RemoteControlDiscoveryGroupsInventoryOutcome, RemoteControlHostCommandError> {
        match self.groups_for(id) {
            Err(()) => Err(RemoteControlHostCommandError::ApplyFailed),
            Ok(None) => Ok(RemoteControlDiscoveryGroupsInventoryOutcome::UnknownInterface),
            Ok(Some(groups)) => Ok(RemoteControlDiscoveryGroupsInventoryOutcome::Groups(
                RemoteControlDiscoveryGroups::new(groups),
            )),
        }
    }

    pub(super) async fn replace_discovery_groups(
        &self,
        id: InterfaceId,
        groups: DiscoveryGroupSet,
    ) -> Result<RemoteControlDiscoveryGroupsReplaceOutcome, RemoteControlHostCommandError> {
        if let Some(status) = self
            .wifi(id)
            .map_err(|_| RemoteControlHostCommandError::ApplyFailed)?
        {
            return map_apply(status.replace_discovery_groups_and_wait(groups).await);
        }
        if let Some(status) = self
            .bluetooth(id)
            .map_err(|_| RemoteControlHostCommandError::ApplyFailed)?
        {
            return map_apply(status.replace_discovery_groups(groups).await);
        }
        Ok(RemoteControlDiscoveryGroupsReplaceOutcome::UnknownInterface)
    }

    pub(super) fn describe_tcp_client(
        &self,
    ) -> Result<RemoteControlTcpClientStatus, RemoteControlHostCommandError> {
        let Ok(clients) = self.tcp.lock() else {
            return Err(RemoteControlHostCommandError::ApplyFailed);
        };
        let Some(client) = clients.first() else {
            return Ok(RemoteControlTcpClientStatus {
                config: RemoteControlTcpClientConfig::Clear,
                enabled: false,
                connection: ConnectionState::Disconnected,
            });
        };
        let Some(target) = client.target() else {
            return Err(RemoteControlHostCommandError::ApplyFailed);
        };
        let config = tcp_config(&target)?;
        let status = client.status();
        Ok(RemoteControlTcpClientStatus {
            config,
            enabled: status.is_enabled(),
            connection: status.connection(),
        })
    }

    pub(super) fn set_tcp_client(
        &self,
        config: RemoteControlTcpClientConfig,
    ) -> Result<RemoteControlTcpClientOutcome, RemoteControlHostCommandError> {
        let Ok(clients) = self.tcp.lock() else {
            return Err(RemoteControlHostCommandError::ApplyFailed);
        };
        let Some(client) = clients.first() else {
            return Ok(RemoteControlTcpClientOutcome::Failed);
        };
        let Some(current) = client.target() else {
            return Err(RemoteControlHostCommandError::ApplyFailed);
        };
        let status = client.status();
        match config {
            RemoteControlTcpClientConfig::Clear => {
                if current.is_empty() && !status.is_enabled() {
                    return Ok(RemoteControlTcpClientOutcome::Unchanged);
                }
                client.retarget("");
                status.disable();
                Ok(RemoteControlTcpClientOutcome::Applied)
            }
            RemoteControlTcpClientConfig::Target(target) => {
                let endpoint = tcp_endpoint(target)?;
                if current == endpoint && status.is_enabled() {
                    return Ok(RemoteControlTcpClientOutcome::Unchanged);
                }
                if current != endpoint {
                    client.retarget(endpoint);
                }
                status.enable();
                Ok(RemoteControlTcpClientOutcome::Applied)
            }
        }
    }

    fn store_usb(&self, id: InterfaceId, status: TokioInterfaceStatus) {
        let Ok(mut statuses) = self.usb.lock() else {
            return;
        };
        let ids = power_ids(id, status.id());
        if let Some(existing) = statuses
            .iter_mut()
            .find(|current| current.ids.iter().any(|stored| ids.contains(stored)))
        {
            existing.ids = ids;
            existing.status = status;
        } else {
            statuses.push(UsbPower { ids, status });
        }
    }

    fn store_tcp(&self, control: TcpClientControl) {
        let Ok(mut clients) = self.tcp.lock() else {
            return;
        };
        let id = control.status().id();
        if let Some(existing) = clients
            .iter_mut()
            .find(|current| current.status().id() == id)
        {
            *existing = control;
        } else {
            clients.push(control);
        }
    }

    pub(super) fn singleton_group(&self, id: InterfaceId) -> Option<String> {
        let groups = self.groups_for(id).ok()??;
        if groups.len() != 1 {
            return None;
        }
        let name = groups.iter().next().map(|group| group.as_str().to_owned());
        name
    }

    fn groups_for(&self, id: InterfaceId) -> Result<Option<DiscoveryGroupSet>, ()> {
        if let Some(status) = self.wifi(id)? {
            return Ok(Some(status.discovery_groups()));
        }
        if let Some(status) = self.bluetooth(id)? {
            return Ok(Some(status.discovery_groups()));
        }
        Ok(None)
    }

    fn wifi(&self, id: InterfaceId) -> Result<Option<AutoWifiStatus>, ()> {
        let statuses = self.wifi.lock().map_err(|_| ())?;
        if let Some(found) = statuses.iter().find(|status| status.ids.contains(&id)) {
            return Ok(Some(found.status.clone()));
        }
        if id.kind() == Some(InterfaceKind::AutoWifi) && statuses.len() == 1 {
            return Ok(Some(statuses[0].status.clone()));
        }
        Ok(None)
    }

    fn bluetooth(&self, id: InterfaceId) -> Result<Option<BluetoothAutoStatus>, ()> {
        let statuses = self.bluetooth.lock().map_err(|_| ())?;
        Ok(statuses.iter().find(|status| status.id() == id).cloned())
    }

    fn tcp_status(&self, id: InterfaceId) -> Result<Option<TokioInterfaceStatus>, ()> {
        let clients = self.tcp.lock().map_err(|_| ())?;
        Ok(clients
            .iter()
            .find(|client| client.status().id() == id)
            .map(|client| client.status()))
    }

    fn usb(&self, id: InterfaceId) -> Result<Option<TokioInterfaceStatus>, ()> {
        let statuses = self.usb.lock().map_err(|_| ())?;
        if let Some(found) = statuses.iter().find(|status| status.ids.contains(&id)) {
            return Ok(Some(found.status.clone()));
        }
        if matches!(
            id.kind(),
            Some(InterfaceKind::UsbAutoHost | InterfaceKind::UsbAutoDevice)
        ) && statuses.len() == 1
        {
            return Ok(Some(statuses[0].status.clone()));
        }
        Ok(None)
    }
}

fn power_ids(attached: InterfaceId, status: InterfaceId) -> Vec<InterfaceId> {
    if attached == status {
        vec![attached]
    } else {
        vec![attached, status]
    }
}

fn apply_wifi_power(status: &AutoWifiStatus, desired: bool) -> RemoteControlPowerOutcome {
    let outcome = apply_power(
        status.is_enabled(),
        desired,
        || status.enable(),
        || status.disable(),
    );
    for member in status.members() {
        if desired {
            member.enable();
        } else {
            member.disable();
        }
    }
    outcome
}

fn apply_power(
    enabled: bool,
    desired: bool,
    enable: impl FnOnce(),
    disable: impl FnOnce(),
) -> RemoteControlPowerOutcome {
    if enabled == desired {
        RemoteControlPowerOutcome::Unchanged
    } else if desired {
        enable();
        RemoteControlPowerOutcome::Applied
    } else {
        disable();
        RemoteControlPowerOutcome::Applied
    }
}

fn tcp_config(target: &str) -> Result<RemoteControlTcpClientConfig, RemoteControlHostCommandError> {
    if target.is_empty() {
        return Ok(RemoteControlTcpClientConfig::Clear);
    }
    RemoteControlTcpClientTarget::parse(target)
        .map(RemoteControlTcpClientConfig::Target)
        .map_err(|_| RemoteControlHostCommandError::ApplyFailed)
}

fn tcp_endpoint(
    target: RemoteControlTcpClientTarget,
) -> Result<String, RemoteControlHostCommandError> {
    let mut endpoint = String::new();
    target
        .write_endpoint(&mut endpoint)
        .map_err(|_| RemoteControlHostCommandError::ApplyFailed)?;
    Ok(endpoint)
}

fn map_apply(
    outcome: DiscoveryGroupApplyOutcome,
) -> Result<RemoteControlDiscoveryGroupsReplaceOutcome, RemoteControlHostCommandError> {
    match outcome {
        DiscoveryGroupApplyOutcome::Applied => {
            Ok(RemoteControlDiscoveryGroupsReplaceOutcome::Applied)
        }
        DiscoveryGroupApplyOutcome::Unchanged => {
            Ok(RemoteControlDiscoveryGroupsReplaceOutcome::Unchanged)
        }
        DiscoveryGroupApplyOutcome::Failed => Err(RemoteControlHostCommandError::ApplyFailed),
    }
}

#[cfg(test)]
mod tests {
    use personal_rns::interfaces::{InterfaceId, InterfaceKind, INTERFACE_ID_LEN};
    use personal_rns::remote_control::{RemoteControlInterfacePower, RemoteControlPowerOutcome};

    use super::DaemonInterfaceControls;

    #[test]
    fn unregistered_interface_power_is_unknown() {
        let controls = DaemonInterfaceControls::default();
        let id = InterfaceId::new([InterfaceKind::AutoWifi as u8; INTERFACE_ID_LEN]);
        assert_eq!(
            controls.set_power(id, RemoteControlInterfacePower::Off),
            RemoteControlPowerOutcome::UnknownInterface
        );
    }

    #[test]
    fn missing_tcp_client_describes_as_clear_and_rejects_a_new_target() {
        use personal_rns::interfaces::ConnectionState;
        use personal_rns::remote_control::{
            RemoteControlTcpClientConfig, RemoteControlTcpClientOutcome,
            RemoteControlTcpClientStatus,
        };

        let controls = DaemonInterfaceControls::default();
        assert!(matches!(
            controls.describe_tcp_client(),
            Ok(RemoteControlTcpClientStatus {
                config: RemoteControlTcpClientConfig::Clear,
                enabled: false,
                connection: ConnectionState::Disconnected,
            })
        ));
        assert!(matches!(
            controls.set_tcp_client(RemoteControlTcpClientConfig::Clear),
            Ok(RemoteControlTcpClientOutcome::Failed)
        ));
    }

    #[test]
    fn tcp_client_describe_and_set_follow_the_live_dial_target() {
        use std::sync::Arc;

        use personal_rns::interfaces::ConnectionState;
        use personal_rns::remote_control::{
            RemoteControlTcpClientConfig, RemoteControlTcpClientOutcome,
            RemoteControlTcpClientStatus, RemoteControlTcpClientTarget,
        };
        use personal_rns::tcp::TcpClientInterface;
        use personal_rns::RegisteredInterfaceControl;

        let controls = Arc::new(DaemonInterfaceControls::default());
        let interface = TcpClientInterface::new("127.0.0.1:4242".to_string());
        (controls.hook())(RegisteredInterfaceControl::TcpClient(interface.control()));
        assert!(matches!(
            controls.describe_tcp_client(),
            Ok(RemoteControlTcpClientStatus {
                config: RemoteControlTcpClientConfig::Target(_),
                enabled: true,
                connection: ConnectionState::Initializing,
            })
        ));
        let target = RemoteControlTcpClientTarget::parse("10.1.2.3:4242");
        assert!(target.is_ok());
        if let Ok(target) = target {
            assert!(matches!(
                controls.set_tcp_client(RemoteControlTcpClientConfig::Target(target)),
                Ok(RemoteControlTcpClientOutcome::Applied)
            ));
            assert!(matches!(
                controls.set_tcp_client(RemoteControlTcpClientConfig::Target(target)),
                Ok(RemoteControlTcpClientOutcome::Unchanged)
            ));
        }
        let endpoint = match controls.describe_tcp_client() {
            Ok(RemoteControlTcpClientStatus {
                config: RemoteControlTcpClientConfig::Target(target),
                enabled: true,
                ..
            }) => {
                let mut endpoint = String::new();
                let _ = target.write_endpoint(&mut endpoint);
                endpoint
            }
            _ => String::new(),
        };
        assert_eq!(endpoint, "10.1.2.3:4242");
    }

    #[test]
    fn tcp_and_usb_power_disable_the_registered_interface() {
        use std::sync::Arc;

        use personal_rns::interfaces::{ConnectionState, InterfaceId, INTERFACE_ID_LEN};
        use personal_rns::manifold::tokio::TokioInterfaceStatus;
        use personal_rns::remote_control::{
            RemoteControlInterfacePower, RemoteControlPowerOutcome, RemoteControlTcpClientConfig,
            RemoteControlTcpClientStatus,
        };
        use personal_rns::tcp::TcpClientInterface;
        use personal_rns::RegisteredInterfaceControl;

        let controls = Arc::new(DaemonInterfaceControls::default());
        let interface = TcpClientInterface::new("127.0.0.1:4242".to_string());
        let tcp_id = interface.id();
        (controls.hook())(RegisteredInterfaceControl::TcpClient(interface.control()));
        assert_eq!(
            controls.set_power(tcp_id, RemoteControlInterfacePower::Off),
            RemoteControlPowerOutcome::Applied
        );
        assert!(matches!(
            controls.describe_tcp_client(),
            Ok(RemoteControlTcpClientStatus {
                config: RemoteControlTcpClientConfig::Target(_),
                enabled: false,
                connection: ConnectionState::Disabled,
            })
        ));
        assert_eq!(
            controls.set_power(tcp_id, RemoteControlInterfacePower::Off),
            RemoteControlPowerOutcome::Unchanged
        );
        assert_eq!(
            controls.set_power(tcp_id, RemoteControlInterfacePower::On),
            RemoteControlPowerOutcome::Applied
        );

        let mut bytes = [0_u8; INTERFACE_ID_LEN];
        bytes.fill(0xD0);
        let usb_id = InterfaceId::new(bytes);
        let usb = TokioInterfaceStatus::new_accounted(usb_id, ConnectionState::Connected);
        (controls.hook())(RegisteredInterfaceControl::UsbAuto {
            id: usb_id,
            status: usb.clone(),
        });
        assert_eq!(
            controls.set_power(usb_id, RemoteControlInterfacePower::Off),
            RemoteControlPowerOutcome::Applied
        );
        assert!(!usb.is_enabled());
        assert_eq!(
            controls.set_power(usb_id, RemoteControlInterfacePower::On),
            RemoteControlPowerOutcome::Applied
        );
        assert!(usb.is_enabled());
    }

    #[test]
    fn wifi_and_usb_power_follow_the_controller_card_id() {
        use std::sync::Arc;

        use personal_rns::interfaces::{
            ConnectionState, InterfaceId, InterfaceKind, InterfaceStatus,
        };
        use personal_rns::manifold::tokio::TokioInterfaceStatus;
        use personal_rns::wifi_auto::AutoWifi;
        use personal_rns::RegisteredInterfaceControl;

        let controls = Arc::new(DaemonInterfaceControls::default());
        let wifi = AutoWifi::new().status();
        let card = InterfaceId::from_channel_tag(InterfaceKind::AutoWifi, b"Wi-Fi");
        (controls.hook())(RegisteredInterfaceControl::AutoWifi {
            id: card,
            status: wifi.clone(),
        });
        let controller_card = InterfaceId::from_channel_tag(InterfaceKind::AutoWifi, b"controller");
        assert_eq!(
            controls.set_power(controller_card, RemoteControlInterfacePower::Off),
            RemoteControlPowerOutcome::Applied
        );
        assert!(!wifi.is_enabled());
        assert_eq!(
            controls.set_power(wifi.id(), RemoteControlInterfacePower::On),
            RemoteControlPowerOutcome::Applied
        );
        assert!(wifi.is_enabled());

        let usb_id = InterfaceId::new([0xD0; 8]);
        let usb = TokioInterfaceStatus::new_accounted(usb_id, ConnectionState::Connected);
        (controls.hook())(RegisteredInterfaceControl::UsbAuto {
            id: usb_id,
            status: usb.clone(),
        });
        let host_card = InterfaceId::from_channel_tag(InterfaceKind::UsbAutoHost, b"USB");
        assert_eq!(
            controls.set_power(host_card, RemoteControlInterfacePower::Off),
            RemoteControlPowerOutcome::Applied
        );
        assert!(!usb.is_enabled());
    }
}
