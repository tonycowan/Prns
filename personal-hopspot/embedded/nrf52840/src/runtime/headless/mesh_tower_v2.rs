use core::future::Future;

use embassy_futures::join::{join, join3};
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_nrf::gpio::Input;
use embassy_time::{Duration, Timer};
use personal_hopspot_core as hopspot;
use personal_rns::engine::{
    AnnounceAppData, AnnounceNow, AnnounceTarget, EgressTarget, OpenRemoteControlPairing,
    PrnsCommand,
};
use personal_rns::lora::LoRaApplyOutcome;
use personal_rns::remote_control::{
    RemoteControlPairingAttemptTimeout, RemoteControlPairingExpiresAfter,
    RemoteControlPairingPermissions, RemoteControlPairingPublicAppDataBytes,
    RemoteControlRequestSet,
};
use personal_rns::runtime::{RemoteControlPairingControl, RemoteControlTargetPairingConfirmation};
use personal_rns::wire::DestinationHash;

use crate::boards::selected as board;
use board::StatusLed;

use super::remote_control::{take_pending_lora_profile, REMOTE_PAIRING_EVENTS};
use super::{
    Mtx, PrnsNodeHandle, COMMANDS, COMMANDS_CAP, COMPLETION, COMPLETIONS_CAP, LORA_CONTROL,
};

type Handle<'a> = PrnsNodeHandle<'a, Mtx, COMMANDS_CAP, COMPLETIONS_CAP>;

pub(super) const INTERFACE_CAPACITY: usize = 2 + super::bluetooth::MEMBERS;
pub(super) const LANE_COUNT: usize = 3;

const PAIRING_APP_DATA: &[u8] = b"MeshTower";
const CONFIRM_ILLUMINATED: Duration = Duration::from_millis(80);
const CONFIRM_DARK: Duration = Duration::from_millis(320);
const INVITATION_START_ON: Duration = Duration::from_millis(1_200);
const INVITATION_START_OFF: Duration = Duration::from_millis(400);
const INVITATION_ZERO_ON: Duration = Duration::from_millis(400);
const INVITATION_ZERO_OFF: Duration = Duration::from_millis(200);
const INVITATION_PULSE_ON: Duration = Duration::from_millis(80);
const INVITATION_PULSE_OFF: Duration = Duration::from_millis(80);
const INVITATION_DIGIT_GAP: Duration = Duration::from_millis(500);

enum PairingUi {
    Idle,
    Window {
        invitation: u32,
    },
    Confirming {
        confirmation: RemoteControlTargetPairingConfirmation,
    },
}

pub(super) const fn heartbeat_timing() -> &'static super::super::heartbeat::HeartbeatTiming {
    &super::super::heartbeat::NORMAL
}

pub(super) async fn maintain() {
    board::maintain().await;
}

pub(super) fn run<I, L, B>(
    io: I,
    lora: L,
    bluetooth: B,
    button: Input<'static>,
    mut status_led: StatusLed,
    node_page_destination: DestinationHash,
) -> impl Future
where
    I: Future,
    L: Future,
    B: Future,
{
    let handle = Handle::new(COMMANDS.sender(), &COMPLETION);
    let controls = async move {
        let mut pairing = PairingUi::Idle;
        loop {
            apply_pending_lora_profile().await;
            status_led.extinguish();
            match pairing {
                PairingUi::Idle => {
                    match select(
                        board::BUTTON_EVENTS.receive(),
                        idle_heartbeat(&mut status_led),
                    )
                    .await
                    {
                        Either::First(board::ButtonEvent::ShortPress) => {
                            announce(&handle, node_page_destination).await;
                        }
                        Either::First(board::ButtonEvent::LongPress) => {
                            pairing = open_pairing(&handle).await;
                        }
                        Either::Second(()) => {}
                    }
                }
                PairingUi::Window { invitation } => {
                    match select3(
                        board::BUTTON_EVENTS.receive(),
                        REMOTE_PAIRING_EVENTS.receive(),
                        blink_invitation(&mut status_led, invitation),
                    )
                    .await
                    {
                        Either3::First(board::ButtonEvent::ShortPress) | Either3::Third(()) => {
                            pairing = PairingUi::Window { invitation };
                        }
                        Either3::First(board::ButtonEvent::LongPress) => {
                            let _ = handle.close_remote_control_pairing().await;
                            pairing = PairingUi::Idle;
                        }
                        Either3::Second(confirmation) => {
                            pairing = PairingUi::Confirming { confirmation };
                        }
                    }
                }
                PairingUi::Confirming { confirmation } => {
                    match select(
                        board::BUTTON_EVENTS.receive(),
                        confirm_heartbeat(&mut status_led),
                    )
                    .await
                    {
                        Either::First(board::ButtonEvent::ShortPress) => {
                            let _ = handle
                                .approve_remote_control_target_pairing(confirmation.approval())
                                .await;
                            pairing = PairingUi::Idle;
                        }
                        Either::First(board::ButtonEvent::LongPress) => {
                            let _ = handle
                                .reject_remote_control_target_pairing(confirmation.rejection())
                                .await;
                            pairing = PairingUi::Idle;
                        }
                        Either::Second(()) => {
                            pairing = PairingUi::Confirming { confirmation };
                        }
                    }
                }
            }
        }
    };
    join(
        join3(io, bluetooth, lora),
        join(board::drive_button(button), controls),
    )
}

async fn announce(handle: &Handle<'_>, node_page_destination: DestinationHash) {
    while handle
        .issue(PrnsCommand::AnnounceNow(AnnounceNow {
            destination: node_page_destination,
            target: AnnounceTarget::AllInterfaces,
            app_data: AnnounceAppData::Registered,
        }))
        .is_none()
    {
        Timer::after(Duration::from_millis(50)).await;
    }
}

async fn open_pairing(handle: &Handle<'_>) -> PairingUi {
    let open = OpenRemoteControlPairing {
        target: EgressTarget::AllInterfaces,
        expires_after: RemoteControlPairingExpiresAfter::try_from(
            hopspot::REMOTE_CONTROL_PAIRING_EXPIRES_AFTER,
        )
        .expect("pairing window is valid"),
        attempt_timeout: RemoteControlPairingAttemptTimeout::try_from(
            hopspot::REMOTE_CONTROL_PAIRING_ATTEMPT_TIMEOUT,
        )
        .expect("pairing attempt timeout is valid"),
        permissions: RemoteControlPairingPermissions::try_from(RemoteControlRequestSet::all())
            .expect("all remote-control requests are valid"),
        public_app_data: RemoteControlPairingPublicAppDataBytes::try_from(PAIRING_APP_DATA)
            .expect("MeshTower app data fits"),
    };
    match handle.open_remote_control_pairing(open).await {
        Ok(opened) => PairingUi::Window {
            invitation: opened.invitation_code.value(),
        },
        Err(_) => PairingUi::Idle,
    }
}

async fn apply_pending_lora_profile() {
    let Some(profile) = take_pending_lora_profile() else {
        return;
    };
    let _applied = LORA_CONTROL.apply(profile).await == LoRaApplyOutcome::Applied;
}

async fn idle_heartbeat(led: &mut StatusLed) {
    let timing = heartbeat_timing();
    led.illuminate();
    Timer::after(timing.illuminated()).await;
    led.extinguish();
    Timer::after(timing.dark()).await;
}

async fn confirm_heartbeat(led: &mut StatusLed) {
    led.illuminate();
    Timer::after(CONFIRM_ILLUMINATED).await;
    led.extinguish();
    Timer::after(CONFIRM_DARK).await;
}

async fn blink_invitation(led: &mut StatusLed, invitation: u32) {
    led.illuminate();
    Timer::after(INVITATION_START_ON).await;
    led.extinguish();
    Timer::after(INVITATION_START_OFF).await;
    let mut remaining = invitation;
    let mut digits = [0u8; 8];
    for slot in digits.iter_mut().rev() {
        *slot = (remaining & 0xF) as u8;
        remaining >>= 4;
    }
    for digit in digits {
        blink_invitation_digit(led, digit).await;
        Timer::after(INVITATION_DIGIT_GAP).await;
    }
}

async fn blink_invitation_digit(led: &mut StatusLed, digit: u8) {
    if digit == 0 {
        led.illuminate();
        Timer::after(INVITATION_ZERO_ON).await;
        led.extinguish();
        Timer::after(INVITATION_ZERO_OFF).await;
        return;
    }
    for _ in 0..digit {
        led.illuminate();
        Timer::after(INVITATION_PULSE_ON).await;
        led.extinguish();
        Timer::after(INVITATION_PULSE_OFF).await;
    }
}
