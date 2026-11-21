use std::{
    thread,
    time::{Duration, Instant},
};

use artinchip_flash::{
    UpgTransport,
    protocol::cbw_csw::{AicCbw, AicCsw},
    transport::CswPolicy,
};
use nusb::{
    DeviceInfo, Endpoint, MaybeFuture,
    transfer::{Buffer, Bulk, In, Out, TransferError},
};

pub const UPDATER: &str = "33c3:6677";
const TRANSFER_SIZE: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const SHORT_TIMEOUT: Duration = Duration::from_millis(500);
const RECONNECT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct UsbDevice {
    pub bus: String,
    pub address: u8,
    pub path: String,
    pub vid: u16,
    pub pid: u16,
}

impl From<&DeviceInfo> for UsbDevice {
    fn from(device: &DeviceInfo) -> Self {
        Self {
            bus: device.bus_id().to_owned(),
            address: device.device_address(),
            path: device
                .port_chain()
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join("-"),
            vid: device.vendor_id(),
            pid: device.product_id(),
        }
    }
}

impl UsbDevice {
    pub fn matches(&self, id: &str) -> bool {
        format!("{:04x}:{:04x}", self.vid, self.pid).eq_ignore_ascii_case(id)
    }

    pub fn same_port(&self, other: &Self) -> bool {
        !self.path.is_empty() && self.bus == other.bus && self.path == other.path
    }

    #[cfg(target_os = "linux")]
    pub fn linux_address(&self) -> Option<(u8, u8)> {
        Some((self.bus.parse().ok()?, self.address))
    }
}

pub fn devices() -> Result<Vec<UsbDevice>, nusb::Error> {
    Ok(nusb::list_devices()
        .wait()?
        .map(|device| UsbDevice::from(&device))
        .collect())
}

struct Endpoints {
    input: Endpoint<Bulk, In>,
    output: Endpoint<Bulk, Out>,
    device_id: nusb::DeviceId,
    address: u8,
}

impl Endpoints {
    fn open(info: &DeviceInfo) -> Result<Self, nusb::Error> {
        let device = info.open().wait()?;
        let interface = device.detach_and_claim_interface(0).wait()?;
        Ok(Self {
            input: interface.endpoint(0x81)?,
            output: interface.endpoint(0x02)?,
            device_id: info.id(),
            address: info.device_address(),
        })
    }
}

pub struct BulkTransport {
    port: UsbDevice,
    endpoints: Endpoints,
    pending_input: Vec<u8>,
    tag: u32,
}

impl BulkTransport {
    pub fn open(port: UsbDevice) -> Result<Self, String> {
        let device = Self::at_port(&port)?
            .filter(|device| device.device_address() == port.address)
            .ok_or("The bulk updater changed before it could be opened.")?;
        let endpoints = Endpoints::open(&device)
            .map_err(|error| format!("Cannot open bulk updater on port {}: {error}", port.path))?;
        Ok(Self {
            port,
            endpoints,
            pending_input: Vec::new(),
            tag: 0,
        })
    }

    fn at_port(port: &UsbDevice) -> Result<Option<DeviceInfo>, String> {
        let mut devices = nusb::list_devices()
            .wait()
            .map_err(|error| error.to_string())?
            .filter(|info| {
                (info.vendor_id(), info.product_id()) == (0x33c3, 0x6677)
                    && port.same_port(&UsbDevice::from(info))
            });
        let device = devices.next();
        if devices.next().is_some() {
            return Err("Several bulk updater devices matched the selected port.".into());
        }
        Ok(device)
    }

    fn next_tag(&mut self) -> u32 {
        self.tag = self.tag.wrapping_add(1).max(1);
        self.tag
    }

    fn send_wrapper(&mut self, wrapper: &AicCbw) -> Result<(), String> {
        let bytes = wrapper.to_bytes();
        let transfer = self
            .endpoints
            .output
            .transfer_blocking(bytes.into(), TIMEOUT);
        transfer
            .status
            .map_err(|error| format!("Bulk command write failed: {error}"))?;
        if transfer.actual_len != bytes.len() {
            return Err("Short bulk command write.".into());
        }
        Ok(())
    }

    fn send_data(&mut self, data: &[u8]) -> Result<(), String> {
        let retry_until = Instant::now() + Duration::from_secs(10);
        let mut sent = 0;
        while sent < data.len() {
            let end = (sent + TRANSFER_SIZE).min(data.len());
            let transfer = self.endpoints.output.transfer_blocking(
                data[sent..end].into(),
                if sent == 0 {
                    Duration::from_secs(1)
                } else {
                    TIMEOUT
                },
            );
            match transfer.status {
                Ok(()) if transfer.actual_len != 0 => sent += transfer.actual_len,
                // A freshly started updater can briefly NAK the data phase.
                // Retry only if the entire data phase is still unsent.
                Err(TransferError::Cancelled)
                    if sent == 0 && transfer.actual_len == 0 && Instant::now() < retry_until =>
                {
                    thread::sleep(RECONNECT_INTERVAL);
                }
                Ok(()) => return Err("Bulk data write made no progress.".into()),
                Err(error) => {
                    return Err(format!("Bulk data write failed at {sent} bytes: {error}"));
                }
            }
        }
        Ok(())
    }

    fn read_packet(&mut self, timeout: Duration) -> Result<usize, TransferError> {
        let transfer = self
            .endpoints
            .input
            .transfer_blocking(Buffer::new(TRANSFER_SIZE), timeout);
        if transfer.actual_len != 0
            && matches!(transfer.status, Ok(()) | Err(TransferError::Cancelled))
        {
            self.pending_input.extend_from_slice(&transfer.buffer);
            return Ok(transfer.actual_len);
        }
        transfer.status.map(|()| 0)
    }

    fn read_data(&mut self, size: usize) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + TIMEOUT;
        while self.pending_input.len() < size {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "Bulk read timed out after {} of {size} bytes.",
                    self.pending_input.len()
                ));
            }
            self.read_packet(left)
                .map_err(|error| format!("Bulk data read failed: {error}"))?;
        }
        Ok(self.pending_input.drain(..size).collect())
    }

    fn read_status(&mut self, tag: u32, policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        let allow_missing = policy == CswPolicy::AllowMissing;
        let deadline = Instant::now()
            + if allow_missing {
                SHORT_TIMEOUT
            } else {
                TIMEOUT
            };
        loop {
            if let Some(status) = take_status(&mut self.pending_input) {
                if status.tag_val() != tag {
                    continue;
                }
                if !status.is_ok() {
                    return Err(format!(
                        "Bulk command failed (status {}, residue {}).",
                        status.status_val(),
                        status.data_residue_val()
                    ));
                }
                return Ok(Some(status));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.read_packet(left.min(SHORT_TIMEOUT)) {
                Ok(_) | Err(TransferError::Cancelled) => {}
                Err(TransferError::Stall | TransferError::Disconnected) if allow_missing => break,
                Err(error) => return Err(format!("Bulk status read failed: {error}")),
            }
        }
        if allow_missing {
            self.pending_input.clear();
            Ok(None)
        } else {
            Err(format!("Timed out reading bulk status for tag {tag}."))
        }
    }
}

fn take_status(input: &mut Vec<u8>) -> Option<AicCsw> {
    let Some(start) = input.windows(4).position(|bytes| bytes == b"USBS") else {
        // Preserve up to three bytes of a marker split across packets.
        input.drain(..input.len().saturating_sub(3));
        return None;
    };
    input.drain(..start);
    let status = AicCsw::from_bytes(input)?;
    input.drain(..13);
    Some(status)
}

impl UpgTransport for BulkTransport {
    fn write_txn(&mut self, payload: &[u8], policy: CswPolicy) -> Result<Option<AicCsw>, String> {
        let tag = self.next_tag();
        self.send_wrapper(&AicCbw::new_write(tag, payload.len() as u32))?;
        self.send_data(payload)?;
        self.read_status(tag, policy)
    }

    fn read_txn(&mut self, length: u32, policy: CswPolicy) -> Result<Vec<u8>, String> {
        let tag = self.next_tag();
        self.send_wrapper(&AicCbw::new_read(tag, length))?;
        let data = self.read_data(length as usize)?;
        self.read_status(tag, policy)?;
        Ok(data)
    }

    fn reconnect(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut disappeared = false;
        let mut last_error = String::new();
        while Instant::now() < deadline {
            match Self::at_port(&self.port) {
                Ok(None) => disappeared = true,
                Ok(Some(info))
                    if disappeared
                        || info.id() != self.endpoints.device_id
                        || info.device_address() != self.endpoints.address =>
                {
                    match Endpoints::open(&info) {
                        Ok(endpoints) => {
                            self.endpoints = endpoints;
                            self.pending_input.clear();
                            thread::sleep(Duration::from_millis(120));
                            return Ok(());
                        }
                        Err(error) => last_error = format!(" {error}"),
                    }
                }
                Ok(Some(_)) => {}
                Err(error) => last_error = format!(" {error}"),
            }
            thread::sleep(RECONNECT_INTERVAL);
        }
        Err(format!(
            "The bulk updater did not reconnect on port {}.{last_error}",
            self.port.path
        ))
    }

    fn drain_rx(&mut self, timeout: Duration, max_bytes: usize) -> Result<(), String> {
        self.pending_input.clear();
        let mut drained = 0;
        while drained < max_bytes {
            match self.read_packet(timeout) {
                Ok(0) | Err(TransferError::Cancelled) => break,
                Ok(bytes) => {
                    drained += bytes;
                    self.pending_input.clear();
                }
                Err(error) => {
                    return Err(format!("Could not drain the bulk input endpoint: {error}"));
                }
            }
        }
        Ok(())
    }

    fn transport_name(&self) -> &'static str {
        "USB bulk"
    }

    fn max_write_chunk(&self, _: u32) -> usize {
        TRANSFER_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::take_status;

    #[test]
    fn retains_a_split_status_and_leaves_the_next_packet_buffered() {
        let mut input = b"staleUS".to_vec();
        assert!(take_status(&mut input).is_none());
        input.extend_from_slice(b"BS\x03\x00\x00");
        assert!(take_status(&mut input).is_none());
        input.extend_from_slice(b"\x00\x00\x00\x00\x00\x00next");
        let status = take_status(&mut input).unwrap();
        assert!(status.is_ok());
        assert_eq!(status.tag_val(), 3);
        assert_eq!(input, b"next");
    }
}
