// See the "macOS permissions note" in README.md before running this on macOS
// Big Sur or later.

use btleplug::api::{Central, CharPropFlags, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::{Manager, PeripheralId};
use byteorder::{BigEndian, LittleEndian, WriteBytesExt};
use futures::stream::StreamExt;
use std::error::Error;
use std::time::Duration;
use tokio::time;
use uuid::Uuid;

/// Only devices whose name contains this string will be tried.
const PERIPHERAL_NAME_MATCH_FILTER: &str = "Neuro";
/// UUID of the characteristic for which we should subscribe to notifications.
const NOTIFY_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x6e400002_b534_f393_67a9_e50e24dccA9e);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();

    let manager = Manager::new().await?;
    let adapter_list = manager.adapters().await?;
    if adapter_list.is_empty() {
        eprintln!("No Bluetooth adapters found");
    }

    for adapter in adapter_list.iter() {
        println!("Starting scan...");
        adapter
            .start_scan(ScanFilter::default())
            .await
            .expect("Can't scan BLE adapter for connected devices...");
        time::sleep(Duration::from_secs(2)).await;
        let peripherals = adapter.peripherals().await?;

        if peripherals.is_empty() {
            eprintln!("->>> BLE peripheral devices were not found, sorry. Exiting...");
        } else {
            // All peripheral devices in range.
            for peripheral in peripherals.iter() {
                let properties = peripheral.properties().await?;
                let is_connected = peripheral.is_connected().await?;
                let local_name = properties
                    .unwrap()
                    .local_name
                    .unwrap_or(String::from("(peripheral name unknown)"));
                println!(
                    "Peripheral {:?} is connected: {:?}",
                    &local_name, is_connected
                );
                // Check if it's the peripheral we want.
                let ledger_uuid = Uuid::parse_str("9c332c11-e85b-3c29-536c-50c4d347ed4d")?;
                if peripheral.id() == PeripheralId::from(ledger_uuid) {
                    println!("Found matching peripheral {:?}...", &local_name);
                    if !is_connected {
                        // Connect if we aren't already connected.
                        if let Err(err) = peripheral.connect().await {
                            eprintln!("Error connecting to peripheral, skipping: {}", err);
                            continue;
                        }
                    }
                    let is_connected = peripheral.is_connected().await?;
                    println!(
                        "Now connected ({:?}) to peripheral {:?}.",
                        is_connected, &local_name
                    );
                    if is_connected {
                        println!("Discover peripheral {:?} services...", local_name);
                        peripheral.discover_services().await?;
                        let write_uuid = Uuid::parse_str("13d63400-2c97-0004-0002-4c6564676572")?;
                        let notify_uuid = Uuid::parse_str("13d63400-2c97-0004-0001-4c6564676572")?;

                        println!("Checking characteristic");

                        let write_characteristic = peripheral
                            .characteristics()
                            .into_iter()
                            .find(|x| x.uuid == write_uuid)
                            .unwrap();

                        let notify_characteristic = peripheral
                            .characteristics()
                            .into_iter()
                            .find(|x| x.uuid == notify_uuid)
                            .unwrap();

                        // Subscribe to notifications from the characteristic with the selected
                        // UUID.

                        println!(
                            "Subscribing to characteristic {:?}",
                            notify_characteristic.uuid
                        );
                        peripheral.subscribe(&notify_characteristic).await?;
                        // Print the first 4 notifications received.
                        let mut notification_stream = peripheral.notifications().await?.take(4);
                        // Process while the BLE connection is not broken or stopped.

                        // send data
                        println!("Attempting to write to {:?}", &local_name);

                        let hrp = b"cosmos";

                        // let data: Vec<u8> = Vec::new();                  let apdu: Vec<u8> = Vec::new();
                        // data.write_u8(0x55)?; // cla
                        // data.write_u8(0x04)?; // ins
                        // data.write_u8(0x01)?; // p1
                        // data.write_u8(0x00)?; // p2

                        // for get_version
                        let mut version_apdu: Vec<u8> = Vec::new();
                        version_apdu.write_u8(0x55)?; // cla
                        version_apdu.write_u8(0x00)?; // ins
                        version_apdu.write_u8(0x00)?; // p1
                        version_apdu.write_u8(0x00)?; // p2
                        version_apdu.write_u8(0x00)?; // len

                        let hrp = b"cosmos";
                        println!("hrp: {:?}", hrp);

                        let mut get_addr_payload: Vec<u8> = Vec::new();
                        get_addr_payload.write_u8(hrp.len() as u8)?; // hrp len
                        get_addr_payload.extend(hrp); // hrp
                        get_addr_payload.write_u32::<BigEndian>(0x44).unwrap();
                        get_addr_payload.write_u32::<BigEndian>(0x118).unwrap();
                        get_addr_payload.write_u32::<BigEndian>(0).unwrap();
                        get_addr_payload.write_u32::<BigEndian>(0).unwrap();
                        get_addr_payload.write_u32::<BigEndian>(0).unwrap();

                        // for GET_ADDR_SECP256K1
                        let mut addr_apdu: Vec<u8> = Vec::new();
                        addr_apdu.write_u8(0x55)?; // cla
                        addr_apdu.write_u8(0x04)?; // ins
                        addr_apdu.write_u8(0x00)?; // p1
                        addr_apdu.write_u8(0x00)?; // p2
                        addr_apdu.write_u8(get_addr_payload.len() as u8)?; // len
                        addr_apdu.extend(get_addr_payload); // payload

                        let mut payload: Vec<u8> = Vec::new();
                        payload.write_u8(0x05)?; // Command Tag
                        payload.write_u16::<BigEndian>(0)?; // Packet Sequence
                        payload.write_u16::<BigEndian>(addr_apdu.len() as u16)?; // Data Length Big endian (2 bytes)
                        payload.extend(addr_apdu); // Cont.
                        peripheral
                            .write(&write_characteristic, &payload, WriteType::WithResponse)
                            .await?;
                        println!("Succeeded writing to {:?}", &local_name);

                        while let Some(data) = notification_stream.next().await {
                            println!(
                                "Received data from {:?} [{:?}]: {:?}",
                                local_name, data.uuid, data.value
                            );
                        }

                        println!("Disconnecting from peripheral {:?}...", local_name);
                        peripheral.disconnect().await?;
                    }
                }
            }
        }
    }
    Ok(())
}
