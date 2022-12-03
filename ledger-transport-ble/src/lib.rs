/*******************************************************************************
*  Licensed under the Apache License, Version 2.0 (the "License");
*  you may not use this file except in compliance with the License.
*  You may obtain a copy of the License at
*
*      http://www.apache.org/licenses/LICENSE-2.0
*
*  Unless required by applicable law or agreed to in writing, software
*  distributed under the License is distributed on an "AS IS" BASIS,
*  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
*  See the License for the specific language governing permissions and
*  limitations under the License.
********************************************************************************/
mod errors;
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager, Peripheral, ScanFilter, WriteType,
};
pub use errors::LedgerBleError;

use byteorder::BigEndian;

use futures::StreamExt;
use log::info;
use uuid::Uuid;

use std::collections::VecDeque;
use std::ops::Deref;

use ledger_transport::{async_trait, APDUAnswer, APDUCommand, Exchange};

use btleplug::platform::{self, PeripheralId};
use byteorder::WriteBytesExt;

use std::time::Duration;
use tokio::time;

//  "9c332c11-e85b-3c29-536c-50c4d347ed4d"
const NANO_X_UUID: Uuid = Uuid::from_fields(
    0x9c332c11,
    0xe85b,
    0x3c29,
    &[0x53, 0x6c, 0x50, 0xc4, 0xd3, 0x47, 0xed, 0x4d],
);

const MTU_COMMAND_TAG: u8 = 0x08;
const APDU_COMMAND_TAG: u8 = 0x05;

#[derive(Debug)]
struct Request {
    command_tag: u8,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct InitialPacket {
    command_tag: u8,
    data_length: u16, // big endian
    payload: Vec<u8>,
}

impl InitialPacket {
    fn serialize(self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.payload.len() + 5);
        buf.write_u8(self.command_tag).unwrap(); // Command Tag
        buf.write_u16::<BigEndian>(0).unwrap(); // Packet Sequence Index
        buf.write_u16::<BigEndian>(self.data_length).unwrap(); // Data Length
        buf.extend(self.payload); // Payload
        buf
    }
}

impl TryFrom<Vec<u8>> for InitialPacket {
    type Error = LedgerBleError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() < 5 {
            return Err(LedgerBleError::InvalidPacket);
        }
        let payload = if value.len() == 5 {
            Vec::new()
        } else {
            value[5..].to_vec()
        };
        Ok(InitialPacket {
            command_tag: value[0],
            data_length: u16::from_be_bytes(value[3..5].try_into().unwrap()),
            payload,
        })
    }
}

#[derive(Debug)]
struct SubsequentPacket {
    command_tag: u8,
    _packet_sequence_index: u16, // big endian
    payload: Vec<u8>,
}

impl SubsequentPacket {
    fn serialize(self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.payload.len() + 3);
        buf.write_u8(self.command_tag).unwrap(); // Command Tag
        buf.write_u16::<BigEndian>(0).unwrap(); // Packet Sequence Index
        buf.extend(self.payload); // Payload
        buf
    }
}

impl TryFrom<Vec<u8>> for SubsequentPacket {
    type Error = LedgerBleError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() < 3 {
            return Err(LedgerBleError::InvalidPacket);
        }
        let payload = if value.len() == 3 {
            Vec::new()
        } else {
            value[3..].to_vec()
        };
        Ok(SubsequentPacket {
            command_tag: value[0],
            _packet_sequence_index: u16::from_be_bytes(value[1..3].try_into().unwrap()),
            payload,
        })
    }
}

#[derive(Debug)]
pub struct TransportNativeBle {
    device: platform::Peripheral,
    write_characteristic: Characteristic,
    notify_characteristic: Characteristic,
}

impl TransportNativeBle {
    fn is_ledger(peripheral: &platform::Peripheral) -> bool {
        peripheral.id() == PeripheralId::from(NANO_X_UUID)
    }

    /// Get a list of ledger devices available
    pub async fn list_ledgers(
        manager: &platform::Manager,
    ) -> Result<Vec<platform::Peripheral>, LedgerBleError> {
        let adapter_list = manager.adapters().await?;

        for adapter in adapter_list.iter() {
            adapter
                .start_scan(ScanFilter::default())
                .await
                .expect("Can't scan BLE adapter for connected devices...");
            time::sleep(Duration::from_secs(2)).await;
            let peripherals = adapter.peripherals().await?;
            if !peripherals.is_empty() {
                return Ok(peripherals.into_iter().filter(Self::is_ledger).collect());
            }
        }
        Ok(vec![])
    }

    /// Create a new BLE transport, returns a vector of available devices
    pub async fn new(manager: &platform::Manager) -> Result<Vec<Self>, LedgerBleError> {
        let ledgers = Self::list_ledgers(manager).await?;
        if ledgers.is_empty() {
            return Err(LedgerBleError::DeviceNotFound);
        }

        let mut output = Vec::new();
        for ledger in ledgers {
            if !ledger.is_connected().await? {
                ledger.connect().await?;
            }
            ledger.discover_services().await?;

            let write_characteristic = ledger
                .characteristics()
                .into_iter()
                .find(|x| x.properties.contains(CharPropFlags::WRITE))
                .unwrap();

            let notify_characteristic = ledger
                .characteristics()
                .into_iter()
                .find(|x| x.properties.contains(CharPropFlags::NOTIFY))
                .unwrap();

            output.push(TransportNativeBle {
                device: ledger,
                write_characteristic,
                notify_characteristic,
            });
        }
        Ok(output)
    }

    async fn write_request(&self, request: Request, mtu: u8) -> Result<(), LedgerBleError> {
        // First we build the individual packets
        let mut payloads = request
            .payload
            .chunks(mtu as usize)
            .collect::<VecDeque<_>>();
        let initial_packet = InitialPacket {
            command_tag: request.command_tag,
            data_length: request.payload.len() as u16,
            payload: payloads.pop_front().unwrap_or_default().to_vec(),
        };
        let subsequent_packets = payloads
            .iter()
            .enumerate()
            .map(|(i, payload)| SubsequentPacket {
                command_tag: 0x03,
                _packet_sequence_index: i as u16 + 1,
                payload: payload.to_vec(),
            })
            .collect::<Vec<_>>();

        // Then we subscribe to the notification channel
        self.device.subscribe(&self.notify_characteristic).await?;

        // Write the initial packet
        info!("writing initial packet {:#?}", initial_packet);
        self.device
            .write(
                &self.write_characteristic,
                &initial_packet.serialize(),
                WriteType::WithResponse,
            )
            .await?;

        // Write the subsequent packets
        for packet in subsequent_packets {
            info!("writing subsequent packet {:#?}", packet);
            self.device
                .write(
                    &self.write_characteristic,
                    &packet.serialize(),
                    WriteType::WithResponse,
                )
                .await?;
        }

        Ok(())
    }

    async fn read_response(&self) -> Result<Vec<u8>, LedgerBleError> {
        let mut buffer = Vec::new();
        let mut stream = self.device.notifications().await?;
        let initial_packet: InitialPacket = stream.next().await.unwrap().value.try_into()?;
        info!("reading initial packet {:#?}", initial_packet);
        buffer.extend(initial_packet.payload);
        while buffer.len() < initial_packet.data_length as usize {
            let packet: SubsequentPacket = stream.next().await.unwrap().value.try_into()?;
            info!("reading subsequent packet {:#?}", packet);
            buffer.extend(packet.payload);
        }
        Ok(buffer)
    }

    async fn get_mtu(&self) -> Result<u8, LedgerBleError> {
        let request = Request {
            command_tag: MTU_COMMAND_TAG,
            payload: Vec::new(),
        };
        self.write_request(request, 0xff).await?;
        self.read_response().await.map(|x| x[0])
    }

    async fn write_apdu(&self, apdu_command: &[u8]) -> Result<(), LedgerBleError> {
        let request = Request {
            command_tag: APDU_COMMAND_TAG,
            payload: apdu_command.to_vec(),
        };
        let mtu = Self::get_mtu(self).await?;
        Self::write_request(self, request, mtu).await?;
        Ok(())
    }

    pub async fn exchange<I: Deref<Target = [u8]>>(
        &self,
        command: &APDUCommand<I>,
    ) -> Result<APDUAnswer<Vec<u8>>, LedgerBleError> {
        self.write_apdu(&command.serialize()).await?;

        let answer = self.read_response().await?;

        APDUAnswer::from_answer(answer).map_err(|_| LedgerBleError::Comm("response was too short"))
    }
}

#[async_trait]
impl Exchange for TransportNativeBle {
    type Error = LedgerBleError;
    type AnswerType = Vec<u8>;

    async fn exchange<I>(
        &self,
        command: &APDUCommand<I>,
    ) -> Result<APDUAnswer<Self::AnswerType>, Self::Error>
    where
        I: Deref<Target = [u8]> + Send + Sync,
    {
        self.exchange(command).await
    }
}

#[cfg(test)]
mod integration_tests {
    use crate::TransportNativeBle;
    use btleplug::platform;
    use byteorder::{LittleEndian, WriteBytesExt};
    use ledger_transport::APDUCommand;
    use log::{debug, info};

    use serial_test::serial;

    fn init_logging() {
        let _ = env_logger::builder().is_test(true).try_init();
    }

    #[tokio::test]
    #[serial]
    async fn ledger_device_path() {
        init_logging();
        let manager = platform::Manager::new().await.unwrap();
        let ledgers = TransportNativeBle::list_ledgers(&manager).await.unwrap();
        for ledger in ledgers {
            info!("{:#?}", ledger);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_get_mtu() {
        let manager = platform::Manager::new().await.unwrap();
        let ledgers = TransportNativeBle::new(&manager).await.unwrap();
        let mtu = ledgers[0].get_mtu().await.unwrap();
        info!("mtu: {}", mtu);
    }

    #[tokio::test]
    #[serial]
    async fn test_get_version() {
        let manager = platform::Manager::new().await.unwrap();
        let ledger = TransportNativeBle::new(&manager)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let command = APDUCommand {
            cla: 0x55,
            ins: 0x00,
            p1: 0x00,
            p2: 0x00,
            data: vec![],
        };
        let res = ledger.exchange(&command).await;

        info!("res: {:#?}", res);
    }

    #[tokio::test]
    #[serial]
    async fn test_get_pub_key() {
        let manager = platform::Manager::new().await.unwrap();
        let ledger = TransportNativeBle::new(&manager)
            .await
            .unwrap()
            .pop()
            .unwrap();

        let hrp = b"cosmos";
        debug!("hrp: {:?}", hrp);

        let mut get_addr_payload: Vec<u8> = Vec::new();
        get_addr_payload.write_u8(hrp.len() as u8).unwrap(); // hrp len
        get_addr_payload.extend(hrp); // hrp
        get_addr_payload
            .write_u32::<LittleEndian>(44 + 0x80000000)
            .unwrap();
        get_addr_payload
            .write_u32::<LittleEndian>(118 + 0x80000000)
            .unwrap();
        get_addr_payload.write_u32::<LittleEndian>(0).unwrap();
        get_addr_payload.write_u32::<LittleEndian>(0).unwrap();
        get_addr_payload.write_u32::<LittleEndian>(0).unwrap();
        let command = APDUCommand {
            cla: 0x55,
            ins: 0x04,
            p1: 0x00,
            p2: 0x00,
            data: get_addr_payload,
        };
        let res = ledger.exchange(&command).await;

        info!("res: {:#?}", res);
    }
}
