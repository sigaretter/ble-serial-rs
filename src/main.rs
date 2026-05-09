use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use btleplug::api::{CharPropFlags, Manager as _, Peripheral as _, WriteType};
use btleplug::platform::Manager;
use clap::Parser;
use futures::StreamExt;
use tokio::signal;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[cfg(unix)]
mod pty;
mod scan;

// Known BLE UART characteristics, in priority order.
// CH9143 / HM-10 / TI CC245x use 0xFFE1 for BOTH notify and write-without-response
// (single characteristic, unlike Nordic UART which has separate RX/TX chars).
const KNOWN_UART_CHARS: &[(&str, &str)] = &[
    ("0000ffe1-0000-1000-8000-00805f9b34fb", "CH9143 / HM-10 / TI CC245x"),
    ("0000ff01-0000-1000-8000-00805f9b34fb", "LithiumBatteryPCB RX notify"),
    ("0000ff02-0000-1000-8000-00805f9b34fb", "LithiumBatteryPCB TX write"),
    ("0000fff1-0000-1000-8000-00805f9b34fb", "Feasycom RX notify"),
    ("0000fff2-0000-1000-8000-00805f9b34fb", "Feasycom TX write"),
    ("6e400002-b5a3-f393-e0a9-e50e24dcca9e", "Nordic UART TX write"),
    ("6e400003-b5a3-f393-e0a9-e50e24dcca9e", "Nordic UART RX notify"),
];

#[derive(Parser, Debug)]
#[command(
    name = "ble-serial",
    about = "Bridge BLE devices to a virtual serial port.\nOptimised for WeAct CH9143 BLE/UART/USB (service 0xFFE0, char 0xFFE1).",
    version
)]
struct Args {
    /// BLE device address/UUID — skips scan and interactive selection
    #[arg(short = 'd', long = "dev")]
    device: Option<String>,

    /// Service UUID to filter scan results (optional)
    #[arg(short = 's', long = "service-uuid")]
    service_uuid: Option<String>,

    /// GATT characteristic for notifications (BLE → host). Auto-detected if omitted.
    #[arg(short = 'r', long = "read-uuid")]
    read_uuid: Option<String>,

    /// GATT characteristic for writes (host → BLE). Auto-detected if omitted.
    #[arg(short = 'w', long = "write-uuid")]
    write_uuid: Option<String>,

    /// Virtual serial port symlink path
    #[arg(short = 'p', long = "port", default_value = "/tmp/ttyBLE")]
    port: std::path::PathBuf,

    /// Max BLE packet size in bytes
    #[arg(short = 'm', long = "mtu", default_value = "20")]
    mtu: usize,

    /// Scan / connect timeout in seconds
    #[arg(short = 't', long = "timeout", default_value = "10.0")]
    timeout: f64,

    /// Use write-with-response (better integrity, higher latency)
    #[arg(long = "write-with-response")]
    write_with_response: bool,

    /// Increase log verbosity (-v = debug, -vv = BLE stack debug)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Logging: -v shows our debug, -vv also shows btleplug internals
    let filter = match args.verbose {
        0 => "info,btleplug=warn",
        1 => "debug,btleplug=warn",
        _ => "debug,btleplug=debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| filter.into()),
        )
        .with_target(false)
        .init();

    let manager = Manager::new().await.context("Failed to initialise Bluetooth")?;
    let adapters = manager.adapters().await.context("Failed to list BLE adapters")?;
    let adapter = adapters.into_iter().next().context("No BLE adapter found")?;

    let timeout = Duration::from_secs_f64(args.timeout);

    let peripheral = scan::find_device(
        &adapter,
        args.device.as_deref(),
        args.service_uuid.as_deref(),
        timeout,
    )
    .await?;

    let name = peripheral
        .properties()
        .await?
        .and_then(|p| p.local_name)
        .unwrap_or_else(|| peripheral.id().to_string());

    info!("Connecting to {} ...", name);
    peripheral.connect().await.context("BLE connect failed")?;
    info!("Connected to {}", name);

    peripheral
        .discover_services()
        .await
        .context("GATT service discovery failed")?;

    let chars = peripheral.characteristics();

    if args.verbose > 0 {
        for c in &chars {
            let label = KNOWN_UART_CHARS
                .iter()
                .find(|(u, _)| u.eq_ignore_ascii_case(&c.uuid.to_string()))
                .map(|(_, l)| *l)
                .unwrap_or("");
            debug!("  char {} {:?}  {}", c.uuid, c.properties, label);
        }
    }

    let write_flag = if args.write_with_response {
        CharPropFlags::WRITE
    } else {
        CharPropFlags::WRITE_WITHOUT_RESPONSE
    };
    let write_type = if args.write_with_response {
        WriteType::WithResponse
    } else {
        WriteType::WithoutResponse
    };

    let write_char =
        find_char(&chars, args.write_uuid.as_deref(), write_flag, "write")?;
    let read_char =
        find_char(&chars, args.read_uuid.as_deref(), CharPropFlags::NOTIFY, "read")?;

    info!("Write characteristic : {}", write_char.uuid);
    info!("Notify characteristic: {}", read_char.uuid);

    peripheral
        .subscribe(&read_char)
        .await
        .context("Failed to subscribe to notifications")?;
    info!("Receiver set up");

    // ── PTY bridge (Unix only) ────────────────────────────────────────────────
    #[cfg(not(unix))]
    compile_error!("Only Unix-family systems (macOS, Linux) are currently supported");

    #[cfg(unix)]
    {
        let (pty, pty_read_rx) = pty::PtyBridge::new(&args.port, args.mtu)?;
        let pty_write_tx = pty.write_sender();
        let notify_uuid = read_char.uuid;
        let mtu = args.mtu;

        let mut notifications = peripheral.notifications().await?;

        // Task A: BLE notifications → PTY
        let ble_to_pty = tokio::spawn(async move {
            while let Some(notif) = notifications.next().await {
                if notif.uuid == notify_uuid {
                    debug!("BLE→PTY {} B", notif.value.len());
                    if pty_write_tx.send(notif.value).is_err() {
                        warn!("PTY write channel closed");
                        break;
                    }
                }
            }
        });

        // Task B: PTY → BLE writes (chunked to MTU)
        let peripheral_c = peripheral.clone();
        let write_char_c = write_char.clone();
        let mut pty_rx = pty_read_rx;
        let pty_to_ble = tokio::spawn(async move {
            while let Some(data) = pty_rx.recv().await {
                for chunk in data.chunks(mtu) {
                    debug!("PTY→BLE {} B", chunk.len());
                    if let Err(e) =
                        peripheral_c.write(&write_char_c, chunk, write_type).await
                    {
                        warn!("BLE write error: {}", e);
                    }
                }
            }
        });

        tokio::select! {
            _ = ble_to_pty => warn!("BLE notification stream ended"),
            _ = pty_to_ble => warn!("PTY reader ended"),
            _ = signal::ctrl_c() => {},
        }

        warn!("Shutdown initiated");
        pty.remove()?;
        let _ = peripheral.disconnect().await;
        info!("Shutdown complete.");
    }

    Ok(())
}

fn find_char(
    chars: &BTreeSet<btleplug::api::Characteristic>,
    uuid_override: Option<&str>,
    required_prop: CharPropFlags,
    label: &str,
) -> Result<btleplug::api::Characteristic> {
    if let Some(uuid_str) = uuid_override {
        let uuid = Uuid::from_str(uuid_str)
            .with_context(|| format!("Invalid {} UUID: {}", label, uuid_str))?;
        chars
            .iter()
            .find(|c| c.uuid == uuid && c.properties.contains(required_prop))
            .cloned()
            .with_context(|| {
                format!(
                    "Characteristic {} does not have {:?} property",
                    uuid_str, required_prop
                )
            })
    } else {
        for (candidate, _) in KNOWN_UART_CHARS {
            let uuid = Uuid::from_str(candidate).unwrap();
            if let Some(c) = chars
                .iter()
                .find(|c| c.uuid == uuid && c.properties.contains(required_prop))
            {
                return Ok(c.clone());
            }
        }
        bail!(
            "No {} characteristic found in known-UUID list. \
             Specify one with --{}-uuid (run with -v to list discovered characteristics)",
            label,
            label
        )
    }
}
