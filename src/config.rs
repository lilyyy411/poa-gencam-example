
use club_kdl::{
    FromKdlValue, KdlDeserialize, KdlIdentifier, KdlNodeExt, KdlValue,
    ToKdlValue,
};
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
#[derive(Debug, Clone, KdlDeserialize)]
// #[kdl(name = "config")]
#[kdl(document)]
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
    #[kdl(child)]
    pub scaler: ScalerConfig,
}

#[derive(Debug, Clone, Copy, KdlDeserialize)]
pub struct Range {
    #[kdl(argument)]
    pub min: u16,
    #[kdl(argument)]
    pub max: u16,
}
#[derive(Debug, Clone, Copy, KdlDeserialize)]
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
#[derive(Debug, KdlDeserialize, Clone)]
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

#[derive(Debug, KdlDeserialize, Copy, Clone)]
pub struct Dims {
    #[kdl(argument)]
    pub width: u32,
    #[kdl(argument)]
    pub height: u32,
}

#[derive(Debug, KdlDeserialize, Clone)]
#[kdl(name = "gps")]
pub struct GpsConfig {
    #[kdl(child, unwrap_arg)]
    pub port: String,
    #[kdl(child, unwrap_arg)]
    pub baud_rate: u32,
    #[kdl(child, unwrap_arg)]
    pub timeout: DurationStr,
}
#[derive(Debug, Clone, KdlDeserialize)]
pub enum ScaleAlg {
    #[kdl(rename = "vpss")]
    Vpss {
        #[kdl(property)]
        device: String,
        #[kdl(flatten)]
        dims: Dims,
    },
    #[kdl(rename = "software")]
    Software {
        #[kdl(flatten)]
        dims: Dims,
        #[kdl(property)]
        alg: SoftwareScalingAlg,
    },
}

/// A wrapper for a deserializable struct to make it deserializable with any node name.
/// This is used because `club_kdl`'s data enums have insane behavior of requiring specific node
/// names for each variant instead of being able to have both behaviors. This make it impossible
/// to use data enums with
#[derive(Clone, Copy, Debug, Hash)]
pub struct AnyNodeName<T>(pub T);
impl<'a, T: for<'b> KdlDeserialize<'b>> KdlDeserialize<'a> for AnyNodeName<T> {
    fn from_kdl_node(node: &'a club_kdl::KdlNode) -> club_kdl::Result<Self> {
        let mut node = node.clone();
        let Some(name) = node.remove(0) else {
            return Err(club_kdl::Error::MissingArgument(0));
        };
        let KdlValue::String(name) = name.value() else {
            return Err(club_kdl::Error::InvalidValue {
                field: "0",
                message: "Expected identifier".into(),
            });
        };
        node.set_name(name.parse::<KdlIdentifier>().map_err(|e| {
            club_kdl::Error::InvalidValue {
                field: "0",
                message: e.to_string(),
            }
        })?);
        Ok(Self(T::from_kdl_node(&node)?))
    }
    fn kdl_matches_any_node() -> bool {
        true
    }
}

#[derive(KdlDeserialize, Debug, Clone, Copy)]
pub enum SoftwareScalingAlg {
    /// Nearest Neighbor
    Nearest,

    /// Linear Filter
    Linear,

    /// Cubic Filter
    Cubic,

    /// Gaussian Filter
    Gaussian,

    /// Lanczos with window 3
    Lanczos,
}

#[derive(KdlDeserialize, Debug, Clone)]
#[kdl(name = "scaler")]
pub struct ScalerConfig {
    #[kdl(child(name = "crop-mode"))]
    pub crop_mode: Option<AnyNodeName<CropMode>>,
    #[kdl(child(name = "up"))]
    pub up: AnyNodeName<ScaleAlg>,
    #[kdl(child(name = "down"))]
    pub down: AnyNodeName<ScaleAlg>,
}

#[derive(KdlDeserialize, Debug, Clone)]
pub enum CropMode {
    #[kdl(rename = "grid")]
    Grid(Dims),
}
