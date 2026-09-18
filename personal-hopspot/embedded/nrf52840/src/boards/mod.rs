use embassy_nrf::nvmc::{Error as NvmcError, Nvmc};
use personal_rns::identity::vault::{FlashVault, FlashVaultError};
use personal_rns::remote_control::{
    load_factory_controller_grant, RemoteControlControllerGrant, RemoteControlControllerGrants,
    RemoteControlInitialControllerGrants, RemoteControlNodeIdentityBootstrap,
    RemoteControlNodeIdentityBootstrapError, REMOTE_CONTROL_IDENTITY_VAULT_SLOTS,
};
use prns_core::entropy::{EntropySource, RuntimeEntropy};
use static_cell::StaticCell;

#[cfg(any(
    feature = "board-t096",
    feature = "board-t114",
    feature = "board-t1000e",
    feature = "board-mesh-tower-v2"
))]
mod status_led;

#[cfg(any(feature = "board-t096", feature = "board-t114"))]
mod button;

#[cfg(any(feature = "board-t096", feature = "board-t114"))]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DisplayIoError {
    Spi,
    NotInitialized,
}

#[cfg(any(feature = "board-t096", feature = "board-t114"))]
mod tft;

pub(crate) type RemoteControlIdentityBootstrapError =
    RemoteControlNodeIdentityBootstrapError<FlashVaultError<NvmcError>>;

pub(crate) struct RemoteControlIdentityLoad {
    pub(crate) bootstrap: RemoteControlNodeIdentityBootstrap,
    pub(crate) factory_grant: Option<RemoteControlControllerGrant>,
}

pub(crate) struct RemoteControlIdentityFlash {
    offset: u32,
}

impl RemoteControlIdentityFlash {
    pub(crate) const fn at(offset: u32) -> Self {
        Self { offset }
    }

    pub(crate) fn load_or_generate<S: EntropySource>(
        &self,
        nvmc: &mut Nvmc<'_>,
        entropy: &mut RuntimeEntropy<S>,
    ) -> Result<RemoteControlIdentityLoad, RemoteControlIdentityBootstrapError> {
        let mut vault =
            FlashVault::<_, REMOTE_CONTROL_IDENTITY_VAULT_SLOTS>::new(nvmc, self.offset);
        let bootstrap = RemoteControlNodeIdentityBootstrap::load_or_generate_with_runtime_entropy(
            &mut vault, entropy,
        )?;
        let factory_grant = load_factory_controller_grant(&vault).unwrap_or(None);
        Ok(RemoteControlIdentityLoad {
            bootstrap,
            factory_grant,
        })
    }
}

pub(crate) fn factory_or_fallback_grants(
    factory: Option<RemoteControlControllerGrant>,
    fallback: RemoteControlInitialControllerGrants<'static>,
) -> RemoteControlInitialControllerGrants<'static> {
    let Some(grant) = factory else {
        return fallback;
    };
    static FACTORY: StaticCell<[RemoteControlControllerGrant; 1]> = StaticCell::new();
    let grants = FACTORY.init([grant]);
    RemoteControlInitialControllerGrants::Grants(
        RemoteControlControllerGrants::try_from(grants.as_slice())
            .expect("the factory controller grant is a single distinct entry"),
    )
}

#[cfg(feature = "board-mesh-tower-v2")]
pub(crate) mod mesh_tower_v2;
#[cfg(feature = "board-t096")]
pub(crate) mod t096;
#[cfg(feature = "board-t1000e")]
pub(crate) mod t1000e;
#[cfg(feature = "board-t114")]
pub(crate) mod t114;
#[cfg(feature = "board-t-echo")]
pub(crate) mod t_echo;

#[cfg(all(
    feature = "board-mesh-tower-v2",
    not(feature = "board-t-echo"),
    not(feature = "board-t096"),
    not(feature = "board-t114"),
    not(feature = "board-t1000e")
))]
pub(crate) use mesh_tower_v2 as selected;
#[cfg(all(
    feature = "board-t096",
    not(feature = "board-t-echo"),
    not(feature = "board-t114"),
    not(feature = "board-t1000e"),
    not(feature = "board-mesh-tower-v2")
))]
#[allow(unused_imports)] // Reserved for the runtime once the bring-up boundary is cleared.
pub(crate) use t096 as selected;
#[cfg(all(
    feature = "board-t1000e",
    not(feature = "board-t-echo"),
    not(feature = "board-t096"),
    not(feature = "board-t114"),
    not(feature = "board-mesh-tower-v2")
))]
pub(crate) use t1000e as selected;
#[cfg(all(
    feature = "board-t114",
    not(feature = "board-t-echo"),
    not(feature = "board-t096"),
    not(feature = "board-t1000e"),
    not(feature = "board-mesh-tower-v2")
))]
pub(crate) use t114 as selected;
#[cfg(all(
    feature = "board-t-echo",
    not(feature = "board-t096"),
    not(feature = "board-t114"),
    not(feature = "board-t1000e"),
    not(feature = "board-mesh-tower-v2")
))]
pub(crate) use t_echo as selected;
