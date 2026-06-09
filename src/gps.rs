use std::{
    io::Read,
    os::fd::{AsFd, BorrowedFd},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::config::GpsConfig;
use eyre::Context;
use serialport::TTYPort;
use smol::Async;
use spdlog::{debug, warn};
use std::os::fd::AsRawFd;
use ublox_gps_tec::{UbxGpsInfo, parse_messages};

/// Automatically implement `AsFd` from `AsRawFd`. TTYPort impls
/// `AsRawFd` for some reason, but not `AsFd`.
struct AsFdFromRaw<T>(T);
impl<T: AsRawFd> AsFd for AsFdFromRaw<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.0.as_raw_fd()) }
    }
}
struct Data {
    stop: AtomicBool,
    info: Mutex<Option<UbxGpsInfo>>,
}
pub struct Gps {
    info: Arc<Data>,
    _background_task: smol::Task<()>,
}
impl Gps {
    pub fn new(config: GpsConfig) -> eyre::Result<Self> {
        let serial = serialport::new(config.port, config.baud_rate).open_native()?;
        let serial =
            Async::new(AsFdFromRaw(serial)).wrap_err("Failed to make serial port async")?;
        let info = Arc::new(Data {
            info: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let background_task = smol::spawn(Self::background_task(serial, info.clone()));
        Ok(Self {
            info,
            _background_task: background_task,
        })
    }
    /// Get the current info from the GPS
    pub fn current_info(&self) -> Option<UbxGpsInfo> {
        self.info
            .info
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    async fn background_task(serial: Async<AsFdFromRaw<TTYPort>>, info: Arc<Data>) {
        match Self::background_task_inner(serial, info.clone()).await {
            Ok(()) => {}
            Err(e) => {
                warn!("Gps connection disconnected: {e}");
                warn!("No location metadata will be attached from this point forwards");
                *info.info.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
        }
    }
    async fn background_task_inner(
        mut serial: Async<AsFdFromRaw<TTYPort>>,
        info: Arc<Data>,
    ) -> eyre::Result<()> {
        while !info.stop.load(Ordering::Relaxed) {
            let mut buf = vec![0; 1024];
            // SAFETY: we don't drop the io source like a degenerate
            let num_read = unsafe { serial.read_with_mut(|port| port.0.read(&mut buf)) }
                .await
                // TODO: handle read failures and reboot the connection
                .wrap_err("Failed to read from port")?;
            if num_read == 0 {
                continue;
            }
            buf.truncate(num_read);
            // Why the hell does this take ownership of the Vec???
            // Thanks Suni, very cool.
            let new_info = match parse_messages(buf) {
                Ok(info) => info,
                Err(e) => {
                    warn!("Failed to parse message from GPS buffer: {e}. Ignoring.");
                    continue;
                }
            };
            debug!("Got new GPS info: {new_info:?}");
            *info.info.lock().unwrap_or_else(PoisonError::into_inner) = Some(new_info);
        }
        Ok(())
    }
}

impl Drop for Gps {
    fn drop(&mut self) {
        self.info.stop.store(true, Ordering::Relaxed);
    }
}
