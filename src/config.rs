use std::time::Duration;

use club_kdl::{FromKdlValue, KdlDeserialize, KdlSerialize, ToKdlValue};
use duration_string::DurationString;
use generic_camera::GenCamRoi;

#[derive(Debug, Clone, Copy)]
pub struct DurationStr(pub DurationString);
impl<'de> FromKdlValue<'de> for DurationStr {
    fn from_kdl_value(value: &'de club_kdl::KdlValue) -> club_kdl::Result<Self> {
        let string = String::from_kdl_value(value)?;
        let data = string
            .parse::<DurationString>()
            .map_err(|e| club_kdl::Error::Custom(e.to_string()))?;
        Ok(Self(data))
    }
}
impl ToKdlValue for &DurationStr {
    fn to_kdl_value(&self) -> club_kdl::KdlValue {
        self.0.to_string().to_kdl_value()
    }
}
#[derive(Debug, Clone, Copy)]
pub struct TargetVal(pub f64);
impl<'de> FromKdlValue<'de> for TargetVal {
    fn from_kdl_value(value: &'de club_kdl::KdlValue) -> club_kdl::Result<Self> {
        Ok(Self(f64::from_kdl_value(value)? / 65536.0))
    }
}
impl ToKdlValue for &TargetVal {
    fn to_kdl_value(&self) -> club_kdl::KdlValue {
        (self.0 * 65536.0).to_kdl_value()
    }
}
#[derive(Debug, Clone, KdlDeserialize, KdlSerialize)]
#[kdl(name = "config")]
pub struct CameraConfig {
    #[kdl(child, unwrap_arg)]
    pub camera: Option<String>,
    #[kdl(child, unwrap_arg)]
    pub cadence: DurationStr,
    #[kdl(child, unwrap_arg, name = "max-exposure")]
    pub max_exposure: DurationStr,
    #[kdl(child, unwrap_arg)]
    pub percentile: f64,
    #[kdl(child, unwrap_arg, name = "max-bin")]
    pub max_bin: i32,
    #[kdl(child, unwrap_arg, name = "target-val")]
    pub target_val: TargetVal,
    #[kdl(child, unwrap_arg, name = "target-uncertainty")]
    pub target_uncertainty: TargetVal,
    #[kdl(child, unwrap_arg)]
    pub gain: Option<f64>,
    #[kdl(child, unwrap_arg, name = "target-temp")]
    pub target_temp: f64,
    #[kdl(child, unwrap_arg, name = "save-fits")]
    pub save_fits: bool,
    #[kdl(child, unwrap_arg, name = "save-png")]
    pub save_png: bool,
    #[kdl(child, unwrap_arg)]
    pub pix8b: bool,
    #[kdl(child)]
    pub change_roi: Option<RoiConfig>,
    #[kdl(child)]
    pub log: LogConfig,
    #[kdl(child)]
    pub gps: Option<GpsConfig>,
}
#[derive(Debug, Clone, Copy, KdlDeserialize, KdlSerialize)]
struct Range {
    #[kdl(argument)]
    min: u16,
    #[kdl(argument)]
    max: u16,
}
#[derive(Debug, Clone, Copy, KdlDeserialize, KdlSerialize)]
#[kdl(name = "roi")]
pub struct RoiConfig {
    #[kdl(child(name = "x"))]
    x: Range,
    #[kdl(child(name = "y"))]
    y: Range,
}
impl RoiConfig {
    pub fn roi(self) -> GenCamRoi {
        let RoiConfig {
            x: Range {
                min: x_min,
                max: x_max,
            },
            y: Range {
                min: y_min,
                max: y_max,
            },
        } = self;
        GenCamRoi {
            x_min,
            y_min,
            width: x_max - x_min,
            height: y_max - y_min,
        }
    }
}
#[derive(Debug, KdlDeserialize, KdlSerialize, Clone)]
#[kdl(name = "log")]
pub struct LogConfig {
    #[kdl(child, unwrap_arg, rename = "max-files")]
    pub max_files: u32,
    #[kdl(child, unwrap_arg, rename = "rotation-time")]
    pub rotation_time: DurationStr,
    #[kdl(child, unwrap_arg, rename = "temperature-monitor-period")]
    pub temperature_period: DurationStr,
    #[kdl(child, unwrap_arg, rename = "dedup-period")]
    pub dedup_period: DurationStr,
    // pub
}
impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            camera: None, // connect to the first camera
            cadence: DurationStr(Duration::from_secs(10).into()),
            max_exposure: DurationStr(Duration::from_secs(120).into()),
            percentile: 95.0,
            max_bin: 4,
            target_val: TargetVal(30000.0 / 65536.0),
            target_uncertainty: TargetVal(2000.0 / 65536.0),
            gain: None, // use the camera default
            target_temp: -10.0,
            save_fits: false,
            save_png: true,
            pix8b: false,
            change_roi: None,
            log: LogConfig {
                max_files: 10,
                rotation_time: DurationStr(Duration::from_mins(15).into()),
                temperature_period: DurationStr(Duration::from_secs(1).into()),
                dedup_period: DurationStr(Duration::from_secs(15).into()),
            },
            gps: Some(GpsConfig {
                port: "/dev/ttyUSB0".into(),
                baud_rate: 115200,
            }),
        }
    }
}

#[derive(Debug, KdlDeserialize, KdlSerialize, Clone)]
#[kdl(name = "gps")]
pub struct GpsConfig {
    #[kdl(child, unwrap_arg)]
    pub port: String,
    #[kdl(child, unwrap_arg)]
    pub baud_rate: u32,
}
