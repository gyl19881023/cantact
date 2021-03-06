//! This crate provides a userspace driver for the CANtact family of
//! Controller Area Network (CAN) devices.
//!
//! The rust library provided by this crate can be used directly to build
//! applications for CANtact. The crate also provides bindings for other
//! langauges.

#![warn(missing_docs)]

use std::fmt;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time;

use crossbeam_channel::RecvError;

use serde::{Deserialize, Serialize};

mod device;
use device::gsusb::*;
use device::*;

pub mod c;
/// Implementation of Python bindings
#[cfg(feature = "python")]
pub mod python;

/// Errors generated by this library
#[derive(Debug)]
pub enum Error {
    /// Errors from device interaction.
    DeviceError(device::Error),
    /// The device could not be found, or the user does not have permissions to access it.
    DeviceNotFound,
    /// Timeout while communicating with the device.
    Timeout,
    /// Attempted to perform an action on a device that is running when this is not allowed.
    Running,
    /// Attempted to perform an action on a device that is not running when this is not allowed.
    NotRunning,
    /// Requested channel index does not exist on device.
    InvalidChannel,
    /// The requested bitrate cannot be set within an acceptable tolerance
    InvalidBitrate(u32),
}
impl From<device::Error> for Error {
    fn from(e: device::Error) -> Error {
        // TODO
        // this could do a much better job of converting
        Error::DeviceError(e)
    }
}

/// Controller Area Network Frame
#[derive(Debug, Clone)]
pub struct Frame {
    /// CAN frame arbitration ID.
    pub can_id: u32,

    /// CAN frame Data Length Code (DLC).
    pub can_dlc: u8,

    /// Device channel used to send or receive the frame.
    pub channel: u8,

    /// Frame data contents.
    pub data: [u8; 8],

    /// Extended (29 bit) arbitration identifier if true,
    /// standard (11 bit) arbitration identifer if false.
    pub ext: bool,

    /// CAN Flexible Data (CAN-FD) frame flag.
    pub fd: bool,

    /// Loopback flag. When true, frame was sent by this device/channel.
    /// False for received frames.
    pub loopback: bool,

    /// Remote Transmission Request (RTR) flag.
    pub rtr: bool,

    /// Timestamp when frame was received
    pub timestamp: Option<time::Duration>,
}
impl Frame {
    // convert to a frame format expected by the device
    fn to_host_frame(&self) -> HostFrame {
        // if frame is extended, set the extended bit in host frame CAN ID
        let mut can_id = if self.ext {
            self.can_id | GSUSB_EXT_FLAG
        } else {
            self.can_id
        };
        // if frame is RTR, set the RTR bit in host frame CAN ID
        can_id = if self.rtr {
            can_id | GSUSB_RTR_FLAG
        } else {
            can_id
        };
        HostFrame {
            echo_id: 1,
            flags: 0,
            reserved: 0,
            can_id,
            can_dlc: self.can_dlc,
            channel: self.channel,
            data: self.data,
        }
    }
    /// Returns a default CAN frame with all values set to zero/false.
    pub fn default() -> Frame {
        Frame {
            can_id: 0,
            can_dlc: 0,
            data: [0u8; 8],
            channel: 0,
            ext: false,
            fd: false,
            loopback: false,
            rtr: false,
            timestamp: None,
        }
    }
    fn from_host_frame(hf: HostFrame) -> Frame {
        // check the extended bit of host frame
        // if set, frame is extended
        let ext = (hf.can_id & GSUSB_EXT_FLAG) > 0;
        // check the RTR bit of host frame
        // if set, frame is RTR
        let rtr = (hf.can_id & GSUSB_RTR_FLAG) > 0;
        // remove flags from CAN ID
        let can_id = hf.can_id & 0x3FFF_FFFF;
        // loopback frame if echo_id is not -1
        let loopback = hf.echo_id != GSUSB_RX_ECHO_ID;

        Frame {
            can_id,
            can_dlc: hf.can_dlc,
            data: hf.data,
            channel: hf.channel,
            ext,
            loopback,
            rtr,
            fd: false, // TODO
            timestamp: None,
        }
    }
}

/// Configuration for a device's CAN channel.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Channel {
    /// Bitrate of the channel in bits/second
    pub bitrate: u32,
    /// When true, channel should be enabled when device starts
    pub enabled: bool,
    /// When true, device is configured in hardware loopback mode
    pub loopback: bool,
    /// When true, device will not transmit on the bus.
    pub monitor: bool,
}

/// Interface for interacting with CANtact devices
pub struct Interface {
    dev: Device,
    running: Arc<RwLock<bool>>,

    can_clock: u32,
    // zero indexed (0 = 1 channel, 1 = 2 channels, etc...)
    channel_count: usize,
    sw_version: u32,
    hw_version: u32,

    channels: Vec<Channel>,
}

impl fmt::Debug for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interface")
            .field("running", &(*self.running.read().unwrap()))
            .field("can_clock", &self.can_clock)
            .field("channel_count", &self.channel_count)
            .field("sw_version", &self.sw_version)
            .field("hw_version", &self.hw_version)
            .field("channels", &self.channels)
            .finish()
    }
}

impl Interface {
    /// Creates a new interface. This always selects the first device found by
    /// libusb. If no device is found, Error::DeviceNotFound is returned.
    pub fn new() -> Result<Interface, Error> {
        let mut dev = match Device::new(UsbContext::new()) {
            Ok(d) => d,
            Err(_) => return Err(Error::DeviceNotFound),
        };

        let dev_config = dev.get_device_config()?;
        let bt_consts = dev.get_bit_timing_consts()?;

        let channel_count = dev_config.icount as usize;

        let mut channels = Vec::new();
        // note: channel_count is zero indexed
        for _ in 0..(channel_count + 1) {
            channels.push(Channel {
                bitrate: 0,
                enabled: true,
                loopback: false,
                monitor: false,
            });
        }

        let i = Interface {
            dev,
            running: Arc::new(RwLock::from(false)),

            channel_count,
            can_clock: bt_consts.fclk_can,
            sw_version: dev_config.sw_version,
            hw_version: dev_config.hw_version,

            channels,
        };

        Ok(i)
    }

    /// Start CAN communication on all configured channels.
    ///
    /// After starting the device, `Interface.send` can be used to send frames.
    /// For every received frame, the `rx_callback` closure will be called.
    pub fn start(
        &mut self,
        mut rx_callback: impl FnMut(Frame) + Sync + Send + 'static,
    ) -> Result<(), Error> {
        // tell the device to go on bus
        for (i, ch) in self.channels.iter().enumerate() {
            let mut flags = 0;
            if ch.monitor {
                flags |= GSUSB_FEATURE_LISTEN_ONLY;
            }
            if ch.loopback {
                flags |= GSUSB_FEATURE_LOOP_BACK;
            }

            let mode = Mode {
                mode: CanMode::Start as u32,
                flags,
            };
            if ch.enabled {
                self.dev.set_mode(i as u16, mode).unwrap();
            }
        }

        {
            *self.running.write().unwrap() = true;
        }

        // rx callback thread
        let can_rx = self.dev.can_rx_recv.clone();
        let running = Arc::clone(&self.running);
        let start_time = time::Instant::now();
        thread::spawn(move || {
            while *running.read().unwrap() {
                match can_rx.recv() {
                    Ok(hf) => {
                        let mut f = Frame::from_host_frame(hf);
                        f.timestamp = Some(time::Instant::now().duration_since(start_time));
                        rx_callback(f)
                    }
                    Err(RecvError) => {
                        // channel disconnected
                        break;
                    }
                }
            }
        });

        self.dev.start_transfers().unwrap();
        Ok(())
    }

    /// Stop CAN communication on all channels.
    pub fn stop(&mut self) -> Result<(), Error> {
        // TODO multi-channel
        for (i, ch) in self.channels.iter().enumerate() {
            let mode = Mode {
                mode: CanMode::Reset as u32,
                flags: 0,
            };
            if ch.enabled {
                self.dev.set_mode(i as u16, mode).unwrap();
            }
        }

        self.dev.stop_transfers().unwrap();
        *self.running.write().unwrap() = false;
        Ok(())
    }

    /// Set bitrate for specified channel to requested bitrate value in bits per second.
    pub fn set_bitrate(&mut self, channel: usize, bitrate: u32) -> Result<(), Error> {
        if channel > self.channel_count {
            return Err(Error::InvalidChannel);
        }

        let bt = calculate_bit_timing(self.can_clock, bitrate)?;
        self.dev
            .set_bit_timing(channel as u16, bt)
            .expect("failed to set bit timing");

        self.channels[channel].bitrate = bitrate;
        Ok(())
    }

    /// Set a custom bit timing for the specified channel.
    pub fn set_bit_timing(
        &mut self,
        channel: usize,
        brp: u32,
        phase_seg1: u32,
        phase_seg2: u32,
        sjw: u32,
    ) -> Result<(), Error> {
        let bt = BitTiming {
            brp,
            prop_seg: 0,
            phase_seg1,
            phase_seg2,
            sjw,
        };
        self.dev
            .set_bit_timing(channel as u16, bt)
            .expect("failed to set bit timing");
        Ok(())
    }

    /// Enable or disable a channel's listen only mode. When this mode is enabled,
    /// the device will not transmit any frames, errors, or acknowledgements.
    pub fn set_monitor(&mut self, channel: usize, enabled: bool) -> Result<(), Error> {
        if channel > self.channel_count {
            return Err(Error::InvalidChannel);
        }
        if *self.running.read().unwrap() {
            return Err(Error::Running);
        }

        self.channels[channel].monitor = enabled;
        Ok(())
    }

    /// Enable or disable a channel's listen only mode. When this mode is enabled,
    /// the device will not transmit any frames, errors, or acknowledgements.
    pub fn set_enabled(&mut self, channel: usize, enabled: bool) -> Result<(), Error> {
        if channel > self.channel_count {
            return Err(Error::InvalidChannel);
        }
        if *self.running.read().unwrap() {
            return Err(Error::Running);
        }

        self.channels[channel].enabled = enabled;
        Ok(())
    }

    /// Enable or disable a channel's loopback mode. When this mode is enabled,
    /// frames sent by the device will be received by the device
    /// *as if they had been sent by another node on the bus*.
    ///
    /// This mode is primarily intended for device testing!
    pub fn set_loopback(&mut self, channel: usize, enabled: bool) -> Result<(), Error> {
        if channel > self.channel_count {
            return Err(Error::InvalidChannel);
        }
        if *self.running.read().unwrap() {
            return Err(Error::Running);
        }

        self.channels[channel].loopback = enabled;
        Ok(())
    }

    /// Send a CAN frame using the device
    pub fn send(&mut self, f: Frame) -> Result<(), Error> {
        if !*self.running.read().unwrap() {
            return Err(Error::NotRunning);
        }

        self.dev.send(f.to_host_frame()).unwrap();
        Ok(())
    }

    /// Returns the number of channels this Interface has
    pub fn channels(&self) -> usize {
        self.channel_count + 1
    }
}

fn calculate_bit_timing(clk: u32, bitrate: u32) -> Result<BitTiming, Error> {
    let max_brp = 32;
    let min_seg1 = 3;
    let max_seg1 = 18;
    let min_seg2 = 2;
    let max_seg2 = 8;
    let tolerances = vec![0.0, 0.1 / 100.0, 0.5 / 100.0];

    for tolerance in tolerances {
        let tmp = clk as f32 / bitrate as f32;
        for brp in 1..(max_brp + 1) {
            let btq = tmp / brp as f32;
            let btq_rounded = btq.round() as u32;

            if btq_rounded >= 4 && btq_rounded <= 32 {
                let err = ((btq / (btq_rounded as f32) - 1.0) * 10000.0).round() / 10000.0;
                if err.abs() > tolerance {
                    // error is not acceptable
                    continue;
                }
            }

            for seg1 in min_seg1..max_seg1 {
                // subtract 1 from seg2 to account for propagation phase
                let seg2 = btq_rounded - seg1 - 1;
                if seg2 < min_seg2 || seg2 > max_seg2 {
                    // invalid seg2 value
                    continue;
                }
                // brp, seg1, and seg2 are all valid
                return Ok(BitTiming {
                    brp,
                    prop_seg: 0,
                    phase_seg1: seg1,
                    phase_seg2: seg2,
                    sjw: 1,
                });
            }
        }
    }
    Err(Error::InvalidBitrate(bitrate))
}

#[allow(dead_code)]
fn effective_bitrate(clk: u32, bt: BitTiming) -> u32 {
    clk / bt.brp / (bt.prop_seg + bt.phase_seg1 + bt.phase_seg2 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_bit_timing() {
        let clk = 24000000;
        let bitrates = vec![1000000, 500000, 250000, 125000, 33333];
        for b in bitrates {
            let bt = calculate_bit_timing(clk, b).unwrap();

            // ensure error < 0.5%
            println!("{:?}", &bt);
            let err = 100.0 * (1.0 - (effective_bitrate(clk, bt) as f32 / b as f32).abs());
            println!("{:?}", err);
            assert!(err < 0.5);
        }
    }
}
