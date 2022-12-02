/*******************************************************************************
*   (c) 2022 Zondax AG
*
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
pub use errors::LedgerHIDError;

use byteorder::BigEndian;

use futures::StreamExt;
use uuid::Uuid;

use std::collections::VecDeque;
use std::ops::Deref;

use ledger_transport::{async_trait, APDUAnswer, APDUCommand, Exchange};

use btleplug::platform::{self, PeripheralId};
use byteorder::WriteBytesExt;

use std::time::Duration;
use tokio::time;

//  "9c332c11-e85b-3c29-536c-50c4d347ed4d"
const LEDGER_UUID: Uuid = Uuid::from_fields(
    0x9c332c11,
    0xe85b,
    0x3c29,
    &[0x53, 0x6c, 0x50, 0xc4, 0xd3, 0x47, 0xed, 0x4d],
);

// const WRITE_UUID: Uuid = Uuid::from_fields(
//     0x13d63400,
//     0x2c97,
//     0x0004,
//     &[0x00, 0x02, 0x4c, 0x65, 0x64, 0x67, 0x65, 0x72],
// );

// const NOTIFY_UUID: Uuid = Uuid::from_fields(
//     0x13d63400,
//     0x2c97,
//     0x0004,
//     &[0x00, 0x01, 0x4c, 0x65, 0x64, 0x67, 0x65, 0x72],
// );

const MTU_COMMAND_TAG: u8 = 0x08;
const APDU_COMMAND_TAG: u8 = 0x05;

// lazy_static! {
//     static ref LEDGER_UUID: Uuid = Uuid::"9c332c11-e85b-3c29-536c-50c4d347ed4d";
//     static ref WRITE_UUID: &str = "13d63400-2c97-0004-0002-4c6564676572";
//     static ref NOTIFY_UUID: &str = "13d63400-2c97-0004-0001-4c6564676572";
// }

// struct MtuResponse {
//     mtu: u8,
// }

// struct ApduResponse {
//     apdu: Vec<u8>,
// }

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
    type Error = LedgerHIDError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() < 5 {
            return Err(LedgerHIDError::InvalidPacket);
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
    packet_sequence_index: u16, // big endian
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
    type Error = LedgerHIDError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() < 3 {
            return Err(LedgerHIDError::InvalidPacket);
        }
        let payload = if value.len() == 3 {
            Vec::new()
        } else {
            value[3..].to_vec()
        };
        Ok(SubsequentPacket {
            command_tag: value[0],
            packet_sequence_index: u16::from_be_bytes(value[1..3].try_into().unwrap()),
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
        peripheral.id() == PeripheralId::from(LEDGER_UUID)
    }

    /// Get a list of ledger devices available
    pub async fn list_ledgers(
        manager: &platform::Manager,
    ) -> Result<Vec<platform::Peripheral>, LedgerHIDError> {
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
        //api.device_list().filter(|dev| Self::is_ledger(dev))
    }

    /// Create a new HID transport, connecting to the first ledger found
    /// # Warning
    /// Opening the same device concurrently will lead to device lock after the first handle is closed
    /// see [issue](https://github.com/ruabmbua/hidapi-rs/issues/81)
    pub async fn new(manager: &platform::Manager) -> Result<Self, LedgerHIDError> {
        let peripheral = Self::list_ledgers(manager)
            .await?
            .pop()
            .ok_or(LedgerHIDError::DeviceNotFound)?;

        Self::open_device(peripheral).await
    }

    /// Open a specific ledger device
    ///
    /// # Note
    /// No checks are made to ensure the device is a ledger device
    ///
    /// # Warning
    /// Opening the same device concurrently will lead to device lock after the first handle is closed
    /// see [issue](https://github.com/ruabmbua/hidapi-rs/issues/81)
    pub async fn open_device(peripheral: platform::Peripheral) -> Result<Self, LedgerHIDError> {
        if !peripheral.is_connected().await? {
            peripheral.connect().await?;
        }

        peripheral.discover_services().await?;

        let write_characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|x| x.properties.contains(CharPropFlags::WRITE))
            .unwrap();

        let notify_characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|x| x.properties.contains(CharPropFlags::NOTIFY))
            .unwrap();

        let ledger = TransportNativeBle {
            device: peripheral,
            write_characteristic,
            notify_characteristic,
        };

        Ok(ledger)
    }

    async fn write_request(&self, request: Request, mtu: u8) -> Result<(), LedgerHIDError> {
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
                packet_sequence_index: i as u16 + 1,
                payload: payload.to_vec(),
            })
            .collect::<Vec<_>>();

        // Then we subscribe to the notification channel
        self.device.subscribe(&self.notify_characteristic).await?;

        // Write the initial packet
        println!("writing initial packet {:?}", initial_packet);
        self.device
            .write(
                &self.write_characteristic,
                &initial_packet.serialize(),
                WriteType::WithResponse,
            )
            .await?;

        // Write the subsequent packets
        for packet in subsequent_packets {
            println!("writing subsequent packet");
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

    async fn read_response(&self) -> Result<Vec<u8>, LedgerHIDError> {
        // Print the first 4 notifications received.
        let mut buffer = Vec::new();
        let mut stream = self.device.notifications().await?;
        let initial_packet: InitialPacket = stream.next().await.unwrap().value.try_into()?;
        println!("initial packet {:?}", initial_packet);
        buffer.extend(initial_packet.payload);
        while buffer.len() < initial_packet.data_length as usize {
            let packet: SubsequentPacket = stream.next().await.unwrap().value.try_into()?;
            println!("packet {:?}", packet);
            buffer.extend(packet.payload);
        }
        Ok(buffer)
    }

    async fn get_mtu(&self) -> Result<u8, LedgerHIDError> {
        let request = Request {
            command_tag: MTU_COMMAND_TAG,
            payload: Vec::new(),
        };
        println!("self {:?}", self);

        self.write_request(request, 0xff).await?;
        self.read_response().await.map(|x| x[0])
    }

    async fn write_apdu(&self, apdu_command: &[u8]) -> Result<(), LedgerHIDError> {
        let request = Request {
            command_tag: APDU_COMMAND_TAG,
            payload: apdu_command.to_vec(),
        };
        let mtu = Self::get_mtu(self).await?;
        Self::write_request(self, request, mtu).await?;
        Ok(())
    }

    // fn read_apdu(&self) -> Result<Vec<u8>, LedgerHIDError> {
    //     let mut buffer = vec![0u8; LEDGER_PACKET_READ_SIZE as usize];
    //     let mut sequence_idx = 0u16;
    //     let mut expected_apdu_len = 0usize;

    //     loop {
    //         let res = device.read_timeout(&mut buffer, LEDGER_TIMEOUT)?;

    //         if (sequence_idx == 0 && res < 7) || res < 5 {
    //             return Err(LedgerHIDError::Comm("Read error. Incomplete header"));
    //         }

    //         let mut rdr = Cursor::new(&buffer);

    //         let rcv_channel = rdr.read_u16::<BigEndian>()?;
    //         let rcv_tag = rdr.read_u8()?;
    //         let rcv_seq_idx = rdr.read_u16::<BigEndian>()?;

    //         if rcv_channel != channel {
    //             return Err(LedgerHIDError::Comm("Invalid channel"));
    //         }
    //         if rcv_tag != 0x05u8 {
    //             return Err(LedgerHIDError::Comm("Invalid tag"));
    //         }

    //         if rcv_seq_idx != sequence_idx {
    //             return Err(LedgerHIDError::Comm("Invalid sequence idx"));
    //         }

    //         if rcv_seq_idx == 0 {
    //             expected_apdu_len = rdr.read_u16::<BigEndian>()? as usize;
    //         }

    //         let available: usize = buffer.len() - rdr.position() as usize;
    //         let missing: usize = expected_apdu_len - apdu_answer.len();
    //         let end_p = rdr.position() as usize + std::cmp::min(available, missing);

    //         let new_chunk = &buffer[rdr.position() as usize..end_p];

    //         info!("[{:3}] << {:}", new_chunk.len(), hex::encode(&new_chunk));

    //         apdu_answer.extend_from_slice(new_chunk);

    //         if apdu_answer.len() >= expected_apdu_len {
    //             return Ok(apdu_answer.len());
    //         }

    //         sequence_idx += 1;
    //     }
    // }

    pub async fn exchange<I: Deref<Target = [u8]>>(
        &self,
        command: &APDUCommand<I>,
    ) -> Result<APDUAnswer<Vec<u8>>, LedgerHIDError> {
        self.write_apdu(&command.serialize()).await?;

        let answer = self.read_response().await?;

        APDUAnswer::from_answer(answer).map_err(|_| LedgerHIDError::Comm("response was too short"))
    }
}

#[async_trait]
impl Exchange for TransportNativeBle {
    type Error = LedgerHIDError;
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
    use ledger_transport::APDUCommand;
    use log::info;

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
            println!("{:?}", ledger);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_get_mtu() {
        let manager = platform::Manager::new().await.unwrap();

        let ledger = TransportNativeBle::new(&manager).await.unwrap();

        let mtu = ledger.get_mtu().await.unwrap();
        println!("mtu: {}", mtu);
    }

    #[tokio::test]
    #[serial]
    async fn test_get_version() {
        let manager = platform::Manager::new().await.unwrap();
        let ledger = TransportNativeBle::new(&manager).await.unwrap();
        let command = APDUCommand {
            cla: 0x55,
            ins: 0x00,
            p1: 0x00,
            p2: 0x00,
            data: vec![],
        };
        let res = ledger.exchange(&command).await;

        println!("res: {:?}", res);
    }

    #[tokio::test]
    #[serial]
    async fn exchange() {
        use ledger_zondax_generic::{App, AppExt};
        struct Dummy;
        impl App for Dummy {
            const CLA: u8 = 0;
        }

        let manager = platform::Manager::new().await.unwrap();

        init_logging();

        let ledger = TransportNativeBle::new(&manager)
            .await
            .expect("Could not get a device");

        // use device info command that works in the dashboard
        let result = futures::executor::block_on(Dummy::get_device_info(&ledger))
            .expect("Error during exchange");
        info!("{:x?}", result);
    }
}
