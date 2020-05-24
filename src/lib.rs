/// Public API for interacting with CANtact devices.

use rusb::Error as UsbError;
mod device;
use device::*;

/// Implementation of Python bindings for CANtact devices
#[cfg(python)]
pub mod python;

/// Errors generated by this library
pub enum Error {
    /// During setup, the device could not be found on the system.
    DeviceNotFound,
    /// Timeout while communicating with the device.
    Timeout,
}

/// Definition of a CAN frame
pub struct Frame {
    /// CAN frame arbitration ID
    pub can_id: u32,
    /// CAN frame Data Length Code (DLC)
    pub can_dlc: u8,
    /// Device channel used to send or receive the frame
    pub channel: u8,
    /// Frame data contents
    pub data: [u8; 8],
}

/// Public CANtact interface for interacting with devices
pub struct Interface {
    dev: Device,
}
impl Interface {
    pub fn new() -> Interface {
        let i = Interface {
            dev: Device::new().unwrap(),
        };
        // TODO get btconsts
        i
    }

    /// Starts device CAN communication for specified channel
    pub fn start(&self, channel: u16) {
        let mode = Mode{
            mode: CanMode::Start as u32,
            flags: 0,
        };
        self.dev.set_mode(channel, mode);
    }

    /// Stops device CAN communication for specified channel
    pub fn stop(&self, channel: u16) {
        let mode = Mode{
            mode: CanMode::Reset as u32,
            flags: 0,
        };
        self.dev.set_mode(channel, mode).unwrap();
    }

    /// Sets bitrate for specified channel to requested bitrate value in bits per second
    pub fn set_bitrate(&self, channel: u16, bitrate: u32) {
        // TODO compute for bitrate
        let bt = BitTiming {
            prop_seg: 0,
            phase_seg1: 13,
            phase_seg2: 2,
            sjw: 1,
            brp: 6,
        };
        self.dev.set_bit_timing(0, bt).expect("failed to set bit timing");
    }

    /// Receives a single CAN frame from the device
    pub fn recv(&self) -> Option<Frame> {
        let hf = match self.dev.get_frame() {
            Ok(hf) => hf,
            Err(e) if e == UsbError::Timeout => return None,
            Err(_) => return None, // TODO better error handling
        };
        Some(Frame {
            can_id: hf.can_id,
            can_dlc: hf.can_dlc,
            data: hf.data,
            channel: hf.channel,
        })
    }

    /// Sends a single CAN frame using the device
    pub fn send(&self, f: Frame) -> Result<(), Error> {
        let hf = HostFrame {
            echo_id: 1,
            can_id: f.can_id,
            can_dlc: f.can_dlc,
            channel: f.channel,
            flags: 0,
            reserved: 0,
            data: f.data,
        };
        self.dev.send_frame(hf).unwrap(); // TODO error handling
        Ok(())
    }
}