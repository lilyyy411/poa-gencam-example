use std::{
    io::{Write, stdout},
    path::Path,
    process::exit,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use duration_string::DurationString;
use eyre::{Context, ContextCompat, eyre};
use generic_camera::{
    AnyGenCam, CaptureAsync, GenCamCtrl, GenCamDescriptor, GenCamDriver, GenCamError,
    GenCamPixelBpp, PropertyValue, Sleep,
    controls::{AnalogCtrl, DeviceCtrl, ExposureCtrl, SensorCtrl},
    dummy::GenCamDriverDummy,
};
use generic_camera_player_one::Driver;
use image::{DynamicImage, imageops::FilterType};
use jiff::Timestamp;
use larpa::Command;
use refimage::{
    CalcOptExp, Debayer, DemosaicMethod, FitsCompression, FitsWrite, GenericImageOwned, ImageProps,
    OptimumExposure, OptimumExposureBuilder, ToLuma,
};
use smol::{
    Task,
    channel::{Receiver, Sender},
    future::FutureExt,
};
use spdlog::{
    Level, Logger, debug, error, info, log,
    sink::{DedupSink, RotatingFileSink},
    warn,
};

use crate::{
    cli::{CaptureArgs, Cli, Subcommand},
    config::CameraConfig,
    gps::Gps,
    util::SmolSleep,
};
mod cli;
mod config;
mod gps;
mod scaler;
mod util;

fn init_logging(args: &CaptureArgs, cfg: &CameraConfig) -> eyre::Result<Arc<Logger>> {
    spdlog::init_env_level_from("LOG")?;
    let log_sink = RotatingFileSink::builder()
        .base_path(args.log_dir.join("log.log"))
        .rotation_policy(spdlog::sink::RotationPolicy::Period(
            cfg.log.rotation_time.0.into(),
        ))
        .max_files(cfg.log.max_files as _)
        .rotate_on_open(true)
        .build_arc()?;
    let log_sink = DedupSink::builder()
        .sink(log_sink)
        .skip_duration(cfg.log.dedup_period.0.into())
        .build_arc()?;

    let new_logger = spdlog::default_logger().fork_with(|new| {
        new.sinks_mut().push(log_sink);
        Ok(())
    })?;
    spdlog::set_default_logger(new_logger.clone());
    info!("Logging successfully initialized");
    Ok(new_logger)
}

fn main() -> eyre::Result<()> {
    let cli = Cli::from_args();
    match cli.subcommand {
        Subcommand::WriteConfig { path } => {
            let cfg = include_str!("../example-config.kdl");
            if path == Path::new("--") {
                println!("{cfg}")
            } else {
                std::fs::write(path, &cfg)?;
            }

            Ok(())
        }
        Subcommand::Capture(args) => start_run(args),
    }
}

fn start_run(args: CaptureArgs) -> eyre::Result<()> {
    let config: CameraConfig = club_kdl::from_str(&std::fs::read_to_string(&args.config)?)?;
    let logger = init_logging(&args, &config)?;

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || {
            info!("Interrupted. Stopping.");
            running.store(false, Ordering::Relaxed);
        })?;
    };

    info!("Successfully set ctrl+C handler");
    smol::block_on(async move {
        loop {
            let res = do_run(
                args.clone(),
                config.clone(),
                logger.clone(),
                running.clone(),
            )
            .await;
            let still_running = running.load(Ordering::Relaxed);
            if still_running {
                match res {
                    Ok(()) => info!("Inner loop was stopped. Recovering."),
                    Err(e) => warn!("Inner loop errored: {e}. Recovering."),
                }
                SmolSleep.sleep(Duration::from_secs(1)).await;
            } else {
                info!("Exiting!");
                break;
            }
        }
        Ok(())
    })
}

async fn do_run(
    args: CaptureArgs,
    config: CameraConfig,
    logger: Arc<Logger>,
    running: Arc<AtomicBool>,
) -> eyre::Result<()> {
    debug!("{:>7}: {config:#?}", "Config");
    debug!("{:>7}: {args:#?}", "Args");
    // SAFETY: We only have one driver instance and the user must
    // make sure they aren't stupid and connect to the same camera twice
    let mut driver = unsafe { Driver::new() };

    let Some(mut camera) = connect_camera(&mut driver, &config, &logger)? else {
        error!("Did not find requested camera. Exiting.");
        if !logger.should_log(Level::Debug) {
            error!("To view the list of available cameras, enable debug-level logging");
        }
        if !logger.should_log(Level::Trace) {
            error!(
                "For more verbose information about the available cameras, enable trace-level logging"
            )
        }
        exit(1);
    };

    setup_camera(&mut camera, &config)?;

    let task_run = Arc::new(AtomicBool::new(true));

    let gps = config
        .gps
        .as_ref()
        .cloned()
        .map(Gps::new)
        .transpose()
        .wrap_err("Failed to connect to GPS serial")?;
    let temp_task = if let Some(info) = camera.info_handle() {
        let temperature_monitor_task: Task<eyre::Result<()>> = {
            let running = running.clone();
            let task_run = task_run.clone();
            let monitor_period = <_>::from(config.log.temperature_period.0);
            let logger = logger.clone();
            smol::spawn(async move {
                info!("Starting temperature monitor task");
                while task_run.load(Ordering::Relaxed) && running.load(Ordering::Relaxed) {
                    smol::Timer::after(monitor_period).await;
                    let (temp, _) = info
                        .get_property(GenCamCtrl::Device(DeviceCtrl::Temperature))
                        .unwrap_or((PropertyValue::from(-273.15f64), false));
                    let cooler_power = info
                        .get_property(GenCamCtrl::Device(DeviceCtrl::CoolerPower))
                        .unwrap_or((PropertyValue::from(-1i64), false))
                        .0
                        .try_into()
                        .unwrap_or(-1i64);
                    let temp = temp.try_into().unwrap_or(-273.15f64);

                    info!("Camera temperature: {temp:>+05.1} C, Cooler Power: {cooler_power:>3}%");
                    if !logger.should_log(Level::Debug) {
                        _ = stdout().write_all(b"\r");
                    }
                }
                if let Err(e) = info.cancel_capture() {
                    error!("Failed to cancel capture: {e}");
                }
                info!("Exiting temperature monitor task");
                Ok(())
            })
        };
        Some(temperature_monitor_task)
    } else {
        None
    };

    let props = camera.list_properties();
    let exp_prop = props
        .get(&GenCamCtrl::Exposure(ExposureCtrl::ExposureTime))
        .wrap_err("Error getting exposure property")?;
    let exp_ctrl = OptimumExposureBuilder::default()
        .percentile_pix((config.percentile * 0.01) as f32)
        .pixel_tgt(config.target_val.0 as _)
        .pixel_uncertainty(config.target_uncertainty.0 as _)
        .pixel_exclusion(100)
        .min_allowed_exp(
            exp_prop
                .get_min()
                .wrap_err("Property does not contain minimum value")
                .expect("Property does not contain minimum value")
                .try_into()
                .expect("Error getting min exposure"),
        )
        .max_allowed_exp(config.max_exposure.0.into())
        .max_allowed_bin(config.max_bin as u16)
        .build()
        .unwrap();
    info!("====== Capturing ======");
    let (send, recv) = smol::channel::bounded(4);
    // let capture_task = smol::spawn();
    let capture_task = capture_loop(
        &mut camera,
        config.clone(),
        exp_ctrl,
        running.clone(),
        task_run.clone(),
        send,
    );
    let temp_task = async move {
        if let Some(temp_task) = temp_task {
            temp_task.await
        } else {
            smol::future::pending().await
        }
    };
    smol::future::or(capture_task, temp_task)
        .or(save_loop(recv, config.clone(), args.clone(), gps))
        .await?;

    Ok(())
}
async fn capture_loop(
    camera: &mut AnyGenCam,
    config: CameraConfig,
    exp_ctrl: OptimumExposure,
    running: Arc<AtomicBool>,
    task_run: Arc<AtomicBool>,
    save_sender: Sender<(GenericImageOwned, Timestamp)>,
) -> eyre::Result<()> {
    let mut last_save = None::<Instant>;
    while running.load(Ordering::Relaxed) && task_run.load(Ordering::Relaxed) {
        let exp_start = jiff::Timestamp::now();
        let res = flatten_fut(camera.capture_async(SmolSleep)).await;
        let img = match res {
            Ok(img) => img,
            Err(GenCamError::TimedOut) => {
                warn!("Capture timed out");
                continue;
            }
            // ctrl c might've been pressed idk
            Err(GenCamError::ExposureNotStarted) => continue,
            Err(GenCamError::ExposureFailed(e)) => {
                error!("Exposure failed: {e}");
                task_run.store(false, Ordering::Relaxed);
                // TODO: reenumerate...
                return Ok(());
            }
            Err(e) => {
                error!("An error occured while capturing: {e}");
                continue;
            }
        };
        let img = GenericImageOwned::from(img);
        'a: {
            if let Some(exp) = img.get_exposure() {
                let mut img = img.clone();
                // for a significant amount of time. It does <[T]>::sort on a multi-megabyte slice when it should be doing
                // a quickselect.
                let Ok((opt_exp, _)) = smol::unblock(move || {
                    img.to_luma()
                        .map_err(|e| eyre!("Failed to convert image to luma: {e}"))?;
                    img.calc_opt_exp(&exp_ctrl, exp, 1)
                        .map_err(|e| eyre!("Failed to calculate optimal exposure: {e}"))
                })
                .await
                // we need to make sure we don't block the thread since calculating a new optimal exposure may block the thread
                else {
                    warn!("Failed to calculate optimal exposure. Will not update exposure.");
                    break 'a;
                };
                // Fuzzy go br
                if opt_exp.abs_diff(exp) > Duration::from_micros(500) {
                    if let Err(e) =
                        camera.set_property(ExposureCtrl::ExposureTime.into(), &opt_exp.into())
                    {
                        error!("Failed to update exposure time: {e}")
                    } else {
                        info!(
                            "\nExposure changed from {:.6} to {:.6}",
                            DurationString::new(exp),
                            DurationString::new(opt_exp)
                        )
                    }
                }
            } else {
                warn!("No exposure value found for image");
            }
        }

        let now = Instant::now();
        let should_save = last_save.is_none_or(|x| now - x > config.cadence.0);
        if should_save {
            debug!("Sending image to the save task to be saved.");
            if let Err(e) = save_sender.try_send((img, exp_start)) {
                warn!("Failed to send image over save channel: {e}. Image will not be saved.");
                warn!("This means that the save task stopped or cannot keep up.")
            }
            last_save = Some(now);
        }
    }
    Ok(())
}

async fn save_loop(
    recv: Receiver<(GenericImageOwned, Timestamp)>,
    cfg: CameraConfig,
    run: CaptureArgs,
    gps: Option<Gps>,
) -> eyre::Result<()> {
    info!("Starting save loop");
    let prefix = run.save_dir.clone();
    // we don't need to pass the `task_run` atomicbool because when the capture task stops, the sender will hang up
    // the channel and error on recv
    loop {
        let Ok((mut img, exp_start)) = recv.recv().await else {
            break Ok(());
        };
        let prefix = prefix.join(exp_start.strftime("%Y-%m-%d").to_string());
        if !prefix.exists() {
            smol::fs::create_dir_all(&prefix)
                .await
                .wrap_err("Failed to create save dir")?;
        }
        // Attach metadata from the GPS
        if let Some(gps) = gps.as_ref()
            && let Some(info) = gps.current_info()
            && cfg.save_fits
        {
            debug!("Attaching GPS metadata to image");
            let (lat, long, alt) = info.location();
            // I think these are the right keys for long/lat.
            _ = img.insert_key("LON", (long, "Longitude of the capture location (deg)"));
            _ = img.insert_key("LAT", (lat, "Latitude of the capture location (deg)"));
            // not a standard keyword. Just make up something that sounds legit
            _ = img.insert_key("ALT", (alt as f64, "Altitude of the capture location (m)"));
            // not a standard keyword once again. Just make up something that sounds legit
            _ = img.insert_key(
                "ALTMSL",
                (
                    info.msl() as f64,
                    "Altitude of the capture location above mean sea level (m)",
                ),
            );
            // _ =
        }
        let img = Arc::new(img);
        if cfg.save_fits {
            let fits_file = prefix.join(exp_start.strftime("%H-%M-%S%.3f.fits").to_string());
            let fits_2 = fits_file.clone();
            // TODO: add location metadata and gps nonsense
            let img = img.clone();
            match smol::unblock(move || img.write_fits(fits_2, FitsCompression::Rice, true)).await {
                Ok(_) => info!("\nSaved FITS image to {}", fits_file.display()),
                Err(e) => warn!(
                    "\nFailed to save FITS image to {}: {e}",
                    fits_file.display()
                ),
            };
        }
        let img = if img.color_space().is_bayer() {
            let res = img.debayer(DemosaicMethod::Nearest);
            match res {
                Ok(img) => Arc::new(img),
                Err(e) => {
                    warn!("\nFailed to debayer image: {e}");
                    warn!("Refusing to save");
                    continue;
                }
            }
        } else {
            img
        };
        if cfg.save_png {
            let dimg = DynamicImage::try_from((*img).clone())
                .map_err(|e| eyre!("Failed to convert image: {e}"))?;
            smol::unblock(move || {
                // TODO: use the other scaler
                let dimg = dimg.resize_exact(1024, 1024, FilterType::Nearest);
                let out_path = prefix.join(exp_start.strftime("%H-%M-%S%.3f.png").to_string());
                _ = dimg.save(&out_path).inspect_err(|e| {
                    warn!("Failed to save PNG image to {}: {e}", out_path.display())
                });
            })
            .await
        }
    }
}
async fn flatten_fut<T, E>(f: Result<impl Future<Output = Result<T, E>>, E>) -> Result<T, E> {
    match f {
        Ok(f) => f.await,
        Err(e) => Err(e),
    }
}
fn setup_camera(camera: &mut AnyGenCam, config: &CameraConfig) -> eyre::Result<()> {
    info!("====== Setting up camera ======");
    let is_color = if let Some(color_sensor_prop) = camera
        .info()
        .wrap_err("Failed to get camera info")?
        .info
        .get("Color Sensor")
        && color_sensor_prop.as_bool().unwrap_or_default()
    {
        true
    } else {
        false
    };
    let fmt = match (is_color, config.pix8b) {
        (_, true) => Some(GenCamPixelBpp::Bpp8),
        (true, false) => Some(GenCamPixelBpp::Bpp16),
        // don't change anything. This is what the ASI example did
        (false, false) => None,
    };
    if let Some(fmt) = fmt {
        info!("Setting pixel format to {fmt:?}");
        camera
            .set_property(SensorCtrl::PixelFormat.into(), &fmt.into())
            .wrap_err("Failed to set pixel format")?;
    }

    info!("Setting target temperature to {} C", config.target_temp);
    if let Err(e) = camera.set_property(DeviceCtrl::CoolerTemp.into(), &config.target_temp.into()) {
        warn!("Failed to set target temperature: {e}");
    }

    if let Some(roi) = config.change_roi {
        let config_roi = roi.roi();
        info!(
            "Setting ROI to: ({}, {}) {}x{}",
            config_roi.x_min, config_roi.y_min, config_roi.width, config_roi.height
        );
        let current_roi = camera.get_roi();
        info!(
            "    Current ROI: ({}, {}) {}x{}",
            current_roi.x_min, current_roi.y_min, current_roi.width, current_roi.height
        );
        if let Err(e) = camera.set_roi(&config_roi) {
            warn!("Failed to set ROI: {e}");
        } else {
            let new_roi = camera.get_roi();

            if *new_roi != config_roi {
                warn!(
                    "New ROI is different from the value set in the config due to hardware requirements."
                );
                warn!(
                    "Expected: ({}, {}) {}x{}",
                    config_roi.x_min, config_roi.y_min, config_roi.width, config_roi.height
                );
                warn!(
                    "  Actual: ({}, {}) {}x{}",
                    new_roi.x_min, new_roi.y_min, new_roi.width, new_roi.height
                );
                warn!("It is recommended to change the ROI config to reflect the actual value");
            }
        }
    }
    let current_roi = camera.get_roi();
    info!(
        "ROI: ({}, {}) {}x{}",
        current_roi.x_min, current_roi.y_min, current_roi.width, current_roi.height
    );
    camera
        .set_property(
            ExposureCtrl::ExposureTime.into(),
            &(Duration::from_millis(100).into()),
        )
        .wrap_err("Failed to set exposure time")?;
    if let Some(prop) = camera.list_properties().get(&AnalogCtrl::Gain.into()) {
        info!("Gain Settings: {prop:#?}");
    }

    if let Some(gain) = config.gain {
        info!("Setting gain to {gain}");
        camera
            .set_property(AnalogCtrl::Gain.into(), &gain.into())
            .expect("Error setting gain");
    }
    if let Ok((gain, auto)) = camera.get_property(AnalogCtrl::Gain.into()) {
        info!(
            "Current gain: {:.1} dB, Auto mode: {auto}",
            gain.as_f64().unwrap_or(-1.0),
        );
    }
    // todo have an else case with optimal gain for specific cameras
    info!("Setup complete!");
    Ok(())
}
fn connect_camera(
    driver: &mut Driver,
    config: &CameraConfig,
    logger: &Arc<Logger>,
) -> eyre::Result<Option<AnyGenCam>> {
    info!("======== Choosing camera  ========");
    info!(
        "POA driver reports {} devices available",
        driver.available_devices()
    );
    let desc = if let Some(name) = config.camera.as_deref() {
        info!("Searching for camera with name {name}");
        if name == "dummy" {
            let cam = GenCamDriverDummy {}.connect_first_device()?;
            return Ok(Some(cam));
        }
        let descs = driver.list_devices()?;
        log_descriptors(logger, &descs);
        let selected = descs
            .into_iter()
            .enumerate()
            .find(|(_, x)| x.name.eq_ignore_ascii_case(name));
        let Some((i, selected)) = selected else {
            return Ok(None);
        };
        info!("Chose candidate #{}.", i + 1);
        selected
    } else {
        info!("Choosing first camera");
        let mut descs = driver.list_devices()?;
        if logger.should_log(Level::Debug) {
            log_descriptors(logger, &descs);
        }
        if descs.is_empty() {
            return Ok(None);
        }
        descs.remove(0)
    };
    info!("====== Camera Info ======");
    log_descriptor(Level::Info, Level::Info, logger, &desc);
    if !logger.should_log(Level::Debug) {
        info!(
            "If this info does not match expectations, enable debug-level logging to view the list of all candidates."
        );
    }
    if !logger.should_log(Level::Trace) {
        info!(
            "For more verbose information about the candidate cameras, enable trace-level logging"
        );
    }
    info!("Connecting to camera...");
    let cam = driver.connect_device(&desc)?;
    info!("Success!");
    Ok(Some(cam))
}

fn log_descriptor(
    base_level: Level,
    verbose_level: Level,
    logger: &Arc<Logger>,
    desc: &GenCamDescriptor,
) {
    log!(base_level, "    Name: {}", desc.name);
    log!(base_level, "  Vendor: {}", desc.vendor);
    log!(base_level, "      ID: {}", desc.id);
    if !logger.should_log(verbose_level) {
        return;
    }
    log!(verbose_level, "=== Info ===");
    for (name, value) in &desc.info {
        let v = display_prop!(value);
        log!(verbose_level, "{name:>15}: {v}");
    }
}
fn log_descriptors(logger: &Arc<Logger>, descs: &[GenCamDescriptor]) {
    if !logger.should_log(spdlog::Level::Debug) {
        return;
    }
    for (i, desc) in descs.iter().enumerate() {
        debug!("==== Candidate #{} ====", i + 1);
        log_descriptor(Level::Debug, Level::Trace, logger, desc);
    }
}
