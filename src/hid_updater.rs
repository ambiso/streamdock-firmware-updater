use std::time::Duration;

use async_hid::{AsyncHidRead, AsyncHidWrite, DeviceReader, DeviceWriter};
use tokio::time::timeout;

use crate::{
    Result,
    n4e_image::{Component, N4eImage, PARTITION_TABLE, field, word},
};

const REPORT_SIZE: usize = 1024;
const CHUNK_SIZE: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

const GET_HWINFO: u8 = 0x00;
const RUN_SHELL: u8 = 0x05;
const SET_UPGRADE_CONFIG: u8 = 0x0a;
const END_UPGRADE: u8 = 0x0b;
const SET_COMPONENT: u8 = 0x10;
const GET_BLOCK_SIZE: u8 = 0x11;
const SEND_COMPONENT: u8 = 0x12;
const GET_COMPONENT_CRC: u8 = 0x13;
const GET_BURN_RESULT: u8 = 0x14;
const GET_PARTITION_TABLE: u8 = 0x17;
const READ_COMPONENT: u8 = 0x18;

// The HID SPL uses the ArtInChip CBW/CSW protocol with 1024-byte
// interrupt reports. See the hardware traces in AKP05E-Open-Firmware's
// docs/bootloader-protocol.md and the Luban-Lite bootloader's lib/aicupg.
pub struct HidUpdater {
    reader: DeviceReader,
    writer: DeviceWriter,
    tag: u32,
}

impl HidUpdater {
    pub fn new((reader, writer): (DeviceReader, DeviceWriter)) -> Self {
        Self {
            reader,
            writer,
            tag: 0x1000,
        }
    }

    pub async fn check_layout(&mut self) -> Result<()> {
        let info = self.exchange(GET_HWINFO, &[], 108).await?;
        if info.len() < 104 || field(&info[..8]) != b"HWINFO" || info[40] != 1 {
            return Err(
                "The HID device did not identify itself as an ArtInChip bootloader.".into(),
            );
        }
        let mut storage = [0; 68];
        storage[..7].copy_from_slice(b"spi-nor");
        let table = self
            .exchange(GET_PARTITION_TABLE, &[&storage], 4096)
            .await?;
        check_partition_table(&table)
    }

    pub async fn burn(
        &mut self,
        image: &N4eImage<'_>,
        progress: impl Fn(&Component<'_>),
    ) -> Result<()> {
        self.configure(SET_UPGRADE_CONFIG, 1).await?;
        for component in std::iter::once(&image.info).chain(&image.firmware) {
            progress(component);
            self.exchange(SET_COMPONENT, &[component.metadata], 0)
                .await?;
            let block = self.read_word(GET_BLOCK_SIZE).await?;
            if block == 0 || (!component.partition.is_empty() && block == 1) {
                return Err(
                    format!("Invalid flash block size for {}: {block}.", component.name).into(),
                );
            }
            self.exchange(SEND_COMPONENT, &[component.data], 0).await?;
            let checksum = self.read_word(GET_COMPONENT_CRC).await?;
            let result = self.read_word(GET_BURN_RESULT).await?;
            if checksum != component.checksum || result != 0 {
                return Err(format!(
                    "The dock could not verify {} (CRC {checksum:08x}, status {result}).",
                    component.name
                )
                .into());
            }
        }
        self.configure(END_UPGRADE, 0).await?;
        self.configure(SET_UPGRADE_CONFIG, 3).await?;
        for component in &image.firmware {
            self.verify(component).await?;
        }
        Ok(())
    }

    pub async fn restart(&mut self) -> Result<()> {
        self.exchange(RUN_SHELL, &[b"\x05\x00\x00\x00reset"], 0)
            .await?;
        Ok(())
    }

    async fn verify(&mut self, component: &Component<'_>) -> Result<()> {
        let mut request = [0; 304];
        request[..7].copy_from_slice(b"spi-nor");
        let name = component.partition.as_bytes();
        request[72..72 + name.len()].copy_from_slice(name);
        request[216..224].copy_from_slice(&component.address.to_le_bytes());
        request[224..232].copy_from_slice(&(component.data.len() as u64).to_le_bytes());
        request[240..252].copy_from_slice(b"mtd:required");
        let actual = self
            .exchange(READ_COMPONENT, &[&request], component.data.len())
            .await?;
        if actual != component.data {
            return Err(format!(
                "Readback of {} differs from the input image.",
                component.name
            )
            .into());
        }
        Ok(())
    }

    async fn configure(&mut self, command: u8, mode: u8) -> Result<()> {
        let mut config = [0; 32];
        config[0] = mode;
        self.exchange(command, &[&32u32.to_le_bytes(), &config], 0)
            .await?;
        Ok(())
    }

    async fn read_word(&mut self, command: u8) -> Result<u32> {
        let data = self.exchange(command, &[], 4).await?;
        if data.len() != 4 {
            return Err(
                format!("Invalid data length for ArtInChip command {command:#04x}.").into(),
            );
        }
        Ok(word(&data, 0))
    }

    async fn exchange(&mut self, command: u8, data: &[&[u8]], max_reply: usize) -> Result<Vec<u8>> {
        let length = data.iter().map(|chunk| chunk.len() as u32).sum();
        self.write_transaction(&command_header(*b"UPGC", command, 0, length))
            .await?;
        for data in data {
            for chunk in data.chunks(CHUNK_SIZE) {
                self.write_transaction(chunk).await?;
            }
        }
        let reply = self.read_transaction(16).await?;
        let length = word(&reply, 8);
        if length as usize > max_reply || reply != command_header(*b"UPGR", command, 0, length) {
            return Err(format!("Unexpected response to ArtInChip command {command:#04x}.").into());
        }
        let mut result = Vec::with_capacity(length as usize);
        while result.len() < length as usize {
            let size = (length as usize - result.len()).min(CHUNK_SIZE);
            result.extend(self.read_transaction(size).await?);
        }
        Ok(result)
    }

    async fn write_transaction(&mut self, data: &[u8]) -> Result<()> {
        self.send_wrapper(data.len(), false).await?;
        self.send(data).await?;
        self.check_status().await
    }

    async fn read_transaction(&mut self, size: usize) -> Result<Vec<u8>> {
        self.send_wrapper(size, true).await?;
        let data = self.receive(size).await?;
        self.check_status().await?;
        Ok(data)
    }

    async fn send_wrapper(&mut self, size: usize, read: bool) -> Result<()> {
        self.tag = self.tag.wrapping_add(1);
        let mut wrapper = [0; 31];
        wrapper[..4].copy_from_slice(b"USBC");
        wrapper[4..8].copy_from_slice(&self.tag.to_le_bytes());
        wrapper[8..12].copy_from_slice(&(size as u32).to_le_bytes());
        wrapper[12] = if read { 0x80 } else { 0 };
        wrapper[14] = 1;
        wrapper[15] = if read { 2 } else { 1 };
        self.send(&wrapper).await
    }

    async fn check_status(&mut self) -> Result<()> {
        let status = self.receive(13).await?;
        if &status[..4] != b"USBS" || word(&status, 4) != self.tag || status[12] != 0 {
            return Err(
                format!("ArtInChip transaction failed (status report {status:02x?}).").into(),
            );
        }
        Ok(())
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(REPORT_SIZE) {
            let mut report = [0; REPORT_SIZE + 1];
            report[1..1 + chunk.len()].copy_from_slice(chunk);
            timeout(IO_TIMEOUT, self.writer.write_output_report(&report)).await??;
        }
        Ok(())
    }

    async fn receive(&mut self, size: usize) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(size);
        let mut report = [0; REPORT_SIZE];
        while data.len() < size {
            let read = timeout(IO_TIMEOUT, self.reader.read_input_report(&mut report)).await??;
            if read == 0 {
                return Err("The dock returned an empty HID report.".into());
            }
            data.extend_from_slice(&report[..read.min(size - data.len())]);
        }
        Ok(data)
    }
}

fn check_partition_table(reply: &[u8]) -> Result<()> {
    let table = std::str::from_utf8(field(reply))?.trim();
    let layout = table.strip_prefix("mtd=").unwrap_or(table);
    if layout == PARTITION_TABLE || layout == PARTITION_TABLE.trim_end_matches(';') {
        Ok(())
    } else {
        Err(format!("The dock's flash layout does not match the N4E image: {table:?}.").into())
    }
}

fn command_header(magic: [u8; 4], command: u8, status: u8, length: u32) -> [u8; 16] {
    let mut header = [0; 16];
    header[..4].copy_from_slice(&magic);
    header[4..8].copy_from_slice(&[1, 1, command, status]);
    header[8..12].copy_from_slice(&length.to_le_bytes());
    let checksum = word(&header, 0)
        .wrapping_add(word(&header, 4))
        .wrapping_add(length);
    header[12..].copy_from_slice(&checksum.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::check_partition_table;

    #[test]
    fn accepts_n4e_hid_partition_reply_with_optional_prefix_and_terminator() {
        assert!(
            check_partition_table(
                b"spi0.0:512k(spl),128k(env),128k(env_r),2m(os),1m(rodata),10m(data)\0"
            )
            .is_ok()
        );
        assert!(
            check_partition_table(
                b"mtd=spi0.0:512k(spl),128k(env),128k(env_r),2m(os),1m(rodata),10m(data);\0"
            )
            .is_ok()
        );
        assert!(
            check_partition_table(
                b"spi0.0:256k(spl),128k(env),128k(env_r),2m(os),1m(rodata),10m(data)"
            )
            .is_err()
        );
    }
}
