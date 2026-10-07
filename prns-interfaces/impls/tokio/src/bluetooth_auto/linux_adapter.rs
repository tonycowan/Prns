#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
//! Which BlueZ `hciN` object still names the radio prnsd opened.
//!
//! bluetoothd assigns adapter names in probe order. A restart can remove `hci1` and bring the
//! same dongle back as `hci0`. The binding follows the adapter address, then falls back to
//! BlueZ's own default (`hci0` if it exists, otherwise the lexicographically first name).

use core::time::Duration;

pub(super) const DEFAULT_ADAPTER_NAME: &str = "hci0";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) const ADAPTER_PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// Consecutive probes that must miss before a silent disappearance reopens the adapter.
/// `AdapterRemoved` does not wait for this.
pub(super) const ADAPTER_PROBE_MISSES: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AdapterProbe {
    Present,
    Gone,
}

pub(super) fn classify_adapter_probe(
    bound_name: &str,
    bound_address: [u8; 6],
    names: &[String],
    probed_address: Option<[u8; 6]>,
) -> AdapterProbe {
    if !names.iter().any(|name| name == bound_name) {
        return AdapterProbe::Gone;
    }
    match probed_address {
        Some(address) if address == bound_address => AdapterProbe::Present,
        Some(_) | None => AdapterProbe::Gone,
    }
}

pub(super) fn choose_adapter_name<'a>(
    names: &'a [String],
    preferred: Option<[u8; 6]>,
    addresses: &[(String, [u8; 6])],
) -> Option<&'a str> {
    if let Some(preferred) = preferred {
        if let Some(name) = names.iter().find(|name| {
            addresses
                .iter()
                .any(|(candidate, address)| candidate == *name && *address == preferred)
        }) {
            return Some(name.as_str());
        }
    }
    if names.iter().any(|name| name == DEFAULT_ADAPTER_NAME) {
        return Some(DEFAULT_ADAPTER_NAME);
    }
    names.iter().min().map(String::as_str)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ProbeStreak {
    misses: u8,
}

impl ProbeStreak {
    pub(super) fn observe(self, probe: AdapterProbe) -> (Self, bool) {
        match probe {
            AdapterProbe::Present => (Self { misses: 0 }, false),
            AdapterProbe::Gone => {
                let misses = self.misses.saturating_add(1);
                (Self { misses }, misses >= ADAPTER_PROBE_MISSES)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_live_hci_name_with_the_same_address_is_kept() {
        let present = names(&["hci1"]);
        assert_eq!(
            classify_adapter_probe(
                "hci1",
                [1, 2, 3, 4, 5, 6],
                &present,
                Some([1, 2, 3, 4, 5, 6])
            ),
            AdapterProbe::Present
        );
    }

    #[test]
    fn an_hci_index_that_disappeared_is_gone() {
        let present = names(&["hci0"]);
        assert_eq!(
            classify_adapter_probe("hci1", [1, 2, 3, 4, 5, 6], &present, None),
            AdapterProbe::Gone
        );
    }

    #[test]
    fn the_same_hci_name_with_a_different_address_is_a_different_radio() {
        let present = names(&["hci1"]);
        assert_eq!(
            classify_adapter_probe(
                "hci1",
                [1, 2, 3, 4, 5, 6],
                &present,
                Some([9, 9, 9, 9, 9, 9])
            ),
            AdapterProbe::Gone
        );
    }

    #[test]
    fn a_dongle_that_is_still_hci1_is_not_replaced_by_hci0() {
        let present = names(&["hci0", "hci1"]);
        let addresses = vec![
            ("hci0".to_string(), [8, 8, 8, 8, 8, 8]),
            ("hci1".to_string(), [1, 2, 3, 4, 5, 6]),
        ];
        assert_eq!(
            choose_adapter_name(&present, Some([1, 2, 3, 4, 5, 6]), &addresses),
            Some("hci1")
        );
    }

    #[test]
    fn recovery_follows_the_adapter_address_onto_its_new_hci_index() {
        let present = names(&["hci0", "hci1"]);
        let addresses = vec![
            ("hci0".to_string(), [1, 2, 3, 4, 5, 6]),
            ("hci1".to_string(), [8, 8, 8, 8, 8, 8]),
        ];
        assert_eq!(
            choose_adapter_name(&present, Some([1, 2, 3, 4, 5, 6]), &addresses),
            Some("hci0")
        );
    }

    #[test]
    fn a_missing_previous_address_uses_hci0_when_it_exists() {
        let present = names(&["hci0", "hci1"]);
        assert_eq!(
            choose_adapter_name(&present, Some([1, 2, 3, 4, 5, 6]), &[]),
            Some("hci0")
        );
    }

    #[test]
    fn a_missing_hci0_uses_the_lexicographically_first_adapter() {
        let present = names(&["hci2", "hci1"]);
        assert_eq!(choose_adapter_name(&present, None, &[]), Some("hci1"));
    }

    #[test]
    fn two_consecutive_misses_reopen_and_a_sighting_clears_the_streak() {
        let (streak, reopen) = ProbeStreak::default().observe(AdapterProbe::Gone);
        assert!(!reopen);
        let (streak, reopen) = streak.observe(AdapterProbe::Gone);
        assert!(reopen);
        let (streak, reopen) = streak.observe(AdapterProbe::Present);
        assert!(!reopen);
        assert_eq!(streak, ProbeStreak::default());
    }
}
