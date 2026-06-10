use std::{
    io::{ErrorKind, PipeReader, PipeWriter, Read, pipe},
    os::fd::{AsFd, BorrowedFd},
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::config::GpsConfig;
use eyre::{Context, eyre};
use rand::{RngExt, distr::slice::Choose};
use smol::{Async, Task, io::AsyncWriteExt};
use spdlog::{debug, warn};
use std::os::fd::AsRawFd;
use ublox_gps_tec::{DEFAULT_DELIM, UbxGpsInfo, parse_messages};

/// Automatically implement `AsFd` from `AsRawFd`. TTYPort impls
/// `AsRawFd` for some reason, but not `AsFd`.
struct AsFdFromRaw<T>(T);
impl<T: AsRawFd> AsFd for AsFdFromRaw<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.0.as_raw_fd()) }
    }
}
impl<T: Read> Read for AsFdFromRaw<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        self.0.read_exact(buf)
    }
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        self.0.read_to_end(buf)
    }
    fn read_to_string(&mut self, buf: &mut String) -> std::io::Result<usize> {
        self.0.read_to_string(buf)
    }
    fn read_vectored(&mut self, bufs: &mut [std::io::IoSliceMut<'_>]) -> std::io::Result<usize> {
        self.0.read_vectored(bufs)
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
    #[cfg(test)]
    pub fn mock(timeout: Duration) -> eyre::Result<(Self, Async<PipeWriter>)> {
        let info = Arc::new(Data {
            info: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let (mock, writer) = MockGps::with_writer(timeout)?;
        let task = smol::spawn(Self::background_task(Async::new(mock)?, info.clone()));
        Ok((
            Self {
                info,
                _background_task: task,
            },
            writer,
        ))
    }
    pub fn new(config: GpsConfig) -> eyre::Result<Self> {
        let info = Arc::new(Data {
            info: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let background_task = if config.port.as_str() != "dummy" {
            let serial = serialport::new(config.port, config.baud_rate)
                .timeout(config.timeout.0.into())
                .open_native()?;

            let serial =
                Async::new(AsFdFromRaw(serial)).wrap_err("Failed to make serial port async")?;

            smol::spawn(Self::background_task(serial, info.clone()))
        } else {
            let dummy =
                MockGps::new(config.timeout.0.into()).wrap_err("Failed to make dummy gps")?;
            let dummy = Async::new(dummy).wrap_err("Failed to make dummy gps non-blocking")?;
            smol::spawn(Self::background_task(dummy, info.clone()))
        };

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
    async fn background_task<T: AsFd + Read>(serial: Async<T>, info: Arc<Data>) {
        match Self::background_task_inner(serial, info.clone()).await {
            Ok(()) => {}
            Err(e) => {
                warn!("Gps connection disconnected: {e}");
                warn!("No location metadata will be attached from this point forwards");
                *info.info.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
        }
    }
    async fn background_task_inner<T: AsFd + Read>(
        mut serial: Async<T>,
        info: Arc<Data>,
    ) -> eyre::Result<()> {
        while !info.stop.load(Ordering::Relaxed) {
            let mut buf = Vec::with_capacity(4096);
            // SAFETY: we don't drop the io source like a degenerate.
            if let Err(e) = unsafe { serial.read_with_mut(|port| port.read_to_end(&mut buf)) }.await
                && e.kind() != ErrorKind::TimedOut
            {
                // TODO: handle read failures and reboot the connection
                return Err(eyre!("Failed to read from serial port: {e}"));
            }

            if buf.is_empty() {
                continue;
            }
            // Why the hell does this take ownership of the Vec???
            let new_info = match parse_messages(buf) {
                Ok(info) => info,
                Err(e) => {
                    warn!("Failed to parse message from GPS buffer: {e}. Ignoring.");
                    continue;
                }
            };
            debug!("Got new GPS info: {:?}", new_info.location());
            // We can't be poisoned to begin with.
            *info.info.lock().unwrap_or_else(PoisonError::into_inner) = Some(new_info);
        }
        Ok(())
    }
}

impl Drop for Gps {
    fn drop(&mut self) {
        // Yes, this means that the GPS will try to process one more message
        // than it is supposed to before being dropped, but I think that is
        // fine
        self.info.stop.store(true, Ordering::Relaxed);
    }
}
struct DummyData {
    stop: AtomicBool,
    read_start: Mutex<Option<Instant>>,
    timeout: Duration,
}
struct MockGps {
    read_pipe: PipeReader,
    _background_task: Option<Task<eyre::Result<()>>>,
    data: Arc<DummyData>,
}
impl Drop for MockGps {
    fn drop(&mut self) {
        self.data.stop.store(true, Ordering::Relaxed);
    }
}
impl AsFd for MockGps {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read_pipe.as_fd()
    }
}
static MOCK_MESSAGES: LazyLock<Vec<&'static [u8]>> = LazyLock::new(|| {
    let mut data = split_slice(include_bytes!("../mock-data/gps.bin"), DEFAULT_DELIM);
    _ = data.pop();
    data
});
impl MockGps {
    pub fn new(timeout: Duration) -> eyre::Result<Self> {
        let (mut this, writer) = Self::with_writer(timeout)?;
        let background_task =
            smol::spawn(Self::default_write_task(writer, timeout, this.data.clone()));
        this._background_task = Some(background_task);
        Ok(this)
    }
    pub fn with_writer(timeout: Duration) -> eyre::Result<(Self, Async<PipeWriter>)> {
        let (reader, writer) = pipe()?;
        let writer = Async::new(writer)?;
        let stop = AtomicBool::new(false);
        let data = Arc::new(DummyData {
            timeout,
            stop,
            read_start: Mutex::new(None),
        });
        Ok((
            Self {
                data,
                read_pipe: reader,
                _background_task: None,
            },
            writer,
        ))
    }
    async fn default_write_task(
        mut writer: Async<PipeWriter>,
        timeout: Duration,
        stop: Arc<DummyData>,
    ) -> eyre::Result<()> {
        while !stop.stop.load(Ordering::Relaxed) {
            // let mut rng = thread();
            let timeout =
                timeout.saturating_sub(Duration::from_millis(rand::rng().random_range(0..64)));
            smol::Timer::after(timeout).await;
            let msg = rand::rng().sample(Choose::new(&MOCK_MESSAGES).unwrap());
            writer.write_all(msg).await?;
        }
        Ok(())
    }
    fn start_read_or_timeout(&mut self) -> std::io::Result<()> {
        // we can get away with being this crude since we know we're only going to be having a single active read.
        // We also don't even need to consider the read itself being successful
        match &mut *self
            .data
            .read_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            last @ &mut Some(read_start) if read_start.elapsed() > self.data.timeout => {
                *last = None;
                Err(std::io::Error::new(ErrorKind::TimedOut, ""))
            }
            Some(_) => Ok(()),
            last @ None => {
                *last = Some(Instant::now());
                Ok(())
            }
        }
    }
}
impl Read for MockGps {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.start_read_or_timeout()?;
        self.read_pipe.read(buf)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        self.start_read_or_timeout()?;
        self.read_pipe.read_exact(buf)
    }
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        self.start_read_or_timeout()?;
        self.read_pipe.read_to_end(buf)
    }
    fn read_to_string(&mut self, buf: &mut String) -> std::io::Result<usize> {
        self.start_read_or_timeout()?;
        self.read_pipe.read_to_string(buf)
    }
    fn read_vectored(&mut self, bufs: &mut [std::io::IoSliceMut<'_>]) -> std::io::Result<usize> {
        self.start_read_or_timeout()?;
        self.read_pipe.read_vectored(bufs)
    }
}
fn split_slice(mut orig_data: &[u8], needle: [u8; 8]) -> Vec<&[u8]> {
    let mut out = Vec::with_capacity(orig_data.len() / 8);
    while let Some(p) = orig_data.array_windows::<8>().position(|x| *x == needle) {
        // SAFETY: `p` is in bounds since it is returned as an index for array_windows
        let (head, tail) = unsafe { orig_data.split_at_unchecked(p) };
        out.push(head);
        // SAFETY: `tail` is guaranteed to start with the delimiter
        // and therefore have a length at least 8
        orig_data = unsafe { tail.get_unchecked(8..) };
    }
    out.push(orig_data);
    out
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use smol::io::AsyncWriteExt;
    use spdlog::init_env_level_from;

    use crate::{
        config::{DurationStr, GpsConfig},
        gps::{Gps, MOCK_MESSAGES, split_slice},
    };
    #[test]
    fn mock_gps_same_data() {
        init_env_level_from("LOG").unwrap();
        smol::block_on(async {
            let (gps, mut writer) =
                Gps::mock(Duration::from_millis(100)).expect("Failed to make mock gps");

            let _task = smol::spawn(async move {
                // if you write from the same task, the write will never go through because
                // the message is larger than the internal pipe capacity (i think)
                let message = MOCK_MESSAGES[0];
                writer
                    .write_all(message)
                    .await
                    .expect("Failed to write to pipe");
                // writer.flush().await.unwrap();
                smol::Timer::after(Duration::from_millis(100)).await;
                writer
                    .write_all(MOCK_MESSAGES[1])
                    .await
                    .expect("Failed to write to pipe");
                smol::Timer::after(Duration::from_millis(200)).await;
            });
            // writer.flush().await.expect("failed to flush");

            smol::Timer::after(Duration::from_millis(200)).await;
            let location = gps.current_info().expect("Did not get data").location();
            assert_eq!(
                location,
                (67.8407095, 20.4110155, 413.4),
                "Mismatched location"
            );

            smol::Timer::after(Duration::from_millis(200)).await;
            let location = gps.current_info().expect("Did not get data").location();
            assert_eq!(
                location,
                (67.84070933333334, 20.411015833333334, 413.3),
                "Mismatched location"
            );
        });
    }
    #[test]
    fn does_mock_gps_work_with_random_data() {
        _ = init_env_level_from("LOG").unwrap();
        smol::block_on(async {
            let gps = Gps::new(GpsConfig {
                port: "dummy".into(),
                baud_rate: 0,
                timeout: DurationStr(Duration::from_millis(50).into()),
            })
            .unwrap();
            smol::Timer::after(Duration::from_millis(125)).await;
            for _ in 0..10 {
                smol::Timer::after(Duration::from_millis(125)).await;
                let info = gps.current_info().map(|x| x.location()).unwrap();
                println!("{info:?}",);
            }
        })
    }
}
