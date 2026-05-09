use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use btleplug::api::{Central, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Peripheral};
use tracing::info;
use uuid::Uuid;

pub async fn find_device(
    adapter: &Adapter,
    device_addr: Option<&str>,
    service_uuid: Option<&str>,
    timeout: Duration,
) -> Result<Peripheral> {
    let filter = if let Some(uuid_str) = service_uuid {
        let uuid = Uuid::from_str(uuid_str).context("Invalid service UUID")?;
        ScanFilter { services: vec![uuid] }
    } else {
        ScanFilter::default()
    };

    info!("Scanning for BLE devices ({:.0}s)…", timeout.as_secs_f64());
    adapter.start_scan(filter).await.context("BLE scan start failed")?;
    tokio::time::sleep(timeout).await;
    adapter.stop_scan().await.ok();

    let peripherals = adapter.peripherals().await.context("Failed to list peripherals")?;

    if peripherals.is_empty() {
        bail!("No BLE devices found during scan");
    }

    if let Some(addr) = device_addr {
        let addr_lc = addr.to_lowercase();
        for p in peripherals {
            if p.id().to_string().to_lowercase() == addr_lc {
                return Ok(p);
            }
            // Also try matching by MAC/BDAddr on Linux
            if let Ok(Some(props)) = p.properties().await {
                if props.address.to_string().to_lowercase() == addr_lc {
                    return Ok(p);
                }
            }
        }
        bail!("Device '{}' not found during scan", addr);
    }

    select_device(peripherals).await
}

async fn select_device(peripherals: Vec<Peripheral>) -> Result<Peripheral> {
    // Collect display labels (fetch properties for each device)
    let mut labels: Vec<String> = Vec::with_capacity(peripherals.len());
    for p in &peripherals {
        let props = p.properties().await.ok().flatten();
        let name = props
            .as_ref()
            .and_then(|p| p.local_name.clone())
            .unwrap_or_else(|| "(unnamed)".into());
        let rssi = props
            .as_ref()
            .and_then(|p| p.rssi)
            .map(|r| format!("{:4} dBm", r))
            .unwrap_or_else(|| "  ?      ".into());
        labels.push(format!("{}  {}  {}", rssi, p.id(), name));
    }

    let labels_clone = labels.clone();
    let selected = tokio::task::spawn_blocking(move || {
        inquire::Select::new("Select BLE device to connect:", labels_clone).prompt()
    })
    .await
    .context("Selection task panicked")?
    .context("No device selected")?;

    let idx = labels.iter().position(|l| l == &selected).unwrap();
    Ok(peripherals.into_iter().nth(idx).unwrap())
}
