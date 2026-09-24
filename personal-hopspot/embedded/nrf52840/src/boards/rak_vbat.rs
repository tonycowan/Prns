use embassy_nrf::saadc::{ChannelConfig, Gain, Reference, Saadc};

/// WisBlock VBAT on the RAK19007, read the way Meshtastic reads the RAK4631 and the
/// RAK3401 1 W kit. The base board's divider is always connected to WisBlock AIN0,
/// which is nRF52840 P0.05 / AIN3. Gain 1/5 with the 0.6 V reference makes a 3.0 V
/// full scale, and 1.73 undoes the 1.5 MΩ / 1 MΩ divider.
pub(crate) struct WisblockVbat {
    adc: Saadc<'static, 1>,
}

impl WisblockVbat {
    pub(crate) fn new(adc: Saadc<'static, 1>) -> Self {
        Self { adc }
    }

    pub(crate) async fn sample_millivolts(&mut self) -> u32 {
        let mut sample = [0i16; 1];
        self.adc.sample(&mut sample).await;
        battery_millivolts(sample[0])
    }
}

pub(crate) fn channel<Pin>(pin: Pin) -> ChannelConfig<'static>
where
    Pin: embassy_nrf::saadc::Input + 'static,
{
    let mut channel = ChannelConfig::single_ended(pin);
    channel.reference = Reference::INTERNAL;
    channel.gain = Gain::GAIN1_5;
    channel
}

/// `raw * 1.73 * 3000 / 4096`, with 1.73 * 3000 = 5190.
const fn battery_millivolts(raw: i16) -> u32 {
    let raw = if raw < 0 { 0 } else { raw as u32 };
    raw * 5_190 / 4_096
}

const _: () = {
    assert!(battery_millivolts(0) == 0);
    // 2.52 V at the pin (4.2 V across the 1.5/2.5 divider) is raw 3441 and reads 4360 mV
    // through the 1.73 scale Meshtastic uses instead of the ideal 1.667.
    assert!(battery_millivolts(3_441) == 4_360);
};
