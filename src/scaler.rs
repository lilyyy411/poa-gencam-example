use std::{
    fs::File,
    io,
    mem::{self, ManuallyDrop},
    os::fd::{AsRawFd, FromRawFd},
    thread::available_parallelism,
};

use clic_vdma::{FRAME_BPP, Geometry, VdmaDevice};
use clic_vpss::{ColorFmt, FrameConfig, VpssDevice};
use eyre::{Context, eyre};
use futures_util::StreamExt;
use image::{
    DynamicImage, EncodableLayout, GenericImageView, RgbImage,
    imageops::{self, FilterType},
};
use smol::{
    Async,
    io::{AsyncReadExt, AsyncWriteExt},
    stream,
};

use crate::config::{
    CropMode, Dims, ScaleDims, ScalerConfig, SoftwareScaling, SoftwareScalingAlg, VpssScaling,
};

pub struct Scaler {
    inner: ScalerInner,
    crop_mode: Option<CropMode>,
    max_concurrency: u32,
}
impl Scaler {
    pub fn new(config: ScalerConfig) -> eyre::Result<Self> {
        let max_concurrency = available_parallelism()
            // don't want to blast every core
            .map(|x| (x.get() as u32 * 2 / 3).max(2))
            .unwrap_or(3);
        let (crop_mode, inner) = match config {
            ScalerConfig::Software {
                crop_mode,
                down,
                up,
            } => (
                crop_mode,
                ScalerInner::Software(SoftwareScaler::new(down, up, max_concurrency)),
            ),
            ScalerConfig::Vpss {
                vdma_device,
                crop_mode,
                down,
                up,
            } => (
                crop_mode,
                ScalerInner::Vpss(VpssScaler::new(&vdma_device, down, up)?),
            ),
        };
        let crop_mode = crop_mode.map(|x| x.0);
        Ok(Self {
            inner,
            crop_mode,
            max_concurrency,
        })
    }
    pub fn recommended_concurrency(&self) -> u32 {
        self.max_concurrency
    }
    pub async fn run(&mut self, image: DynamicImage) -> eyre::Result<Vec<RgbImage>> {
        let image = image.into_rgb8();
        let inputs: Vec<_> = match self.crop_mode {
            Some(CropMode::Grid(Dims { width, height })) => {
                make_centered_grid(image, width, height, self.max_concurrency).await
            }
            None => {
                vec![image]
            }
        };
        match &mut self.inner {
            ScalerInner::Software(s) => Ok(s.scale_batch(inputs).await),
            ScalerInner::Vpss(v) => v.scale_batch(inputs).await,
        }
    }
}
enum ScalerInner {
    Software(SoftwareScaler),
    Vpss(VpssScaler),
}

#[derive(Clone, Copy)]
struct SoftwarePass {
    filter: FilterType,
    dims: ScaleDims,
}
impl SoftwarePass {
    pub fn new(scaling: SoftwareScaling) -> Self {
        Self {
            filter: match scaling.alg {
                SoftwareScalingAlg::Cubic => FilterType::CatmullRom,
                SoftwareScalingAlg::Gaussian => FilterType::Gaussian,
                SoftwareScalingAlg::Linear => FilterType::Triangle,
                SoftwareScalingAlg::Lanczos => FilterType::Lanczos3,
                SoftwareScalingAlg::Nearest => FilterType::Nearest,
            },
            dims: scaling.dims.0,
        }
    }
}

pub struct SoftwareScaler {
    down: SoftwarePass,
    up: Option<SoftwarePass>,
    max_concurrency: u32,
}

impl SoftwareScaler {
    pub fn new(down: SoftwareScaling, up: Option<SoftwareScaling>, max_concurrency: u32) -> Self {
        Self {
            down: SoftwarePass::new(down),
            up: up.map(SoftwarePass::new),
            max_concurrency,
        }
    }

    pub async fn scale_batch(
        &mut self,
        images: impl IntoIterator<Item = RgbImage>,
    ) -> Vec<RgbImage> {
        let down = self.down;
        let up = self.up;
        let max_concurrency = self.max_concurrency;
        // Use up to max_concurrency threads to scale the batch of images.
        stream::iter(images.into_iter().map(move |image| {
            let (down_w, down_h) = dims_from(down.dims, &image);
            smol::unblock(move || {
                let mut out = imageops::resize(&image, down_w, down_h, down.filter);
                if let Some(up) = up {
                    let (out_width, out_height) = dims_from(up.dims, &image);
                    out = imageops::resize(&out, out_width, out_height, up.filter);
                }
                out
            })
        }))
        .buffered(max_concurrency as _)
        .collect()
        .await
    }
}

struct AsyncVdma {
    inner: Async<File>,
}
impl AsyncVdma {
    pub fn new(vdma: VdmaDevice) -> io::Result<Self> {
        let file = unsafe { File::from_raw_fd(vdma.as_raw_fd()) };
        std::mem::forget(vdma);
        let inner = Async::new(file)?;
        Ok(Self { inner })
    }
    fn real_inner(&self) -> ManuallyDrop<VdmaDevice> {
        ManuallyDrop::new(unsafe { VdmaDevice::from_raw_fd(self.inner.as_raw_fd()) })
    }
    pub fn set_geometry(&self, geometry: Geometry) -> io::Result<()> {
        // This also does not block (I think)
        self.real_inner().set_geometry(geometry)
    }
    pub async fn read_frame(&self, buf: &mut [u8]) -> io::Result<()> {
        (&self.inner).read_exact(buf).await
    }
    pub async fn write_frame(&self, buf: &[u8]) -> io::Result<()> {
        (&self.inner).write_all(buf).await
    }
}
struct VpssScale {
    dev: VpssDevice,
    dims: ScaleDims,
    nn: bool,
}
impl VpssScale {
    pub fn new(VpssScaling { device, dims, nn }: VpssScaling) -> eyre::Result<Self> {
        let dev = VpssDevice::open(&device)?;
        Ok(Self {
            dev,
            dims: dims.0,
            nn,
        })
    }
    pub fn cfg_for(&self, img: &RgbImage) -> eyre::Result<FrameConfig> {
        let (width, height) = dims_from(self.dims, img);
        Ok(FrameConfig {
            width: width.try_into().wrap_err("Overflow in image width")?,
            height: height.try_into().wrap_err("Overflow in image height")?,
            color: ColorFmt::Rgb,
        })
    }
}
pub struct VpssScaler {
    vdma_device: AsyncVdma,
    // set on the fly based on the size of the inputs
    input_config: Option<FrameConfig>,
    downscale: VpssScale,
    upscale: Option<VpssScale>,
}

impl VpssScaler {
    pub fn new(
        vdma_path: &str,
        downscale: VpssScaling,
        upscale: Option<VpssScaling>,
    ) -> eyre::Result<Self> {
        let vdma_device = VdmaDevice::open(vdma_path).wrap_err("Failed to open vdma device")?;
        let vdma_device =
            AsyncVdma::new(vdma_device).wrap_err("Failed to make vdma device async")?;
        let downscale = VpssScale::new(downscale).wrap_err("Failed to open downscaler")?;
        let upscale = upscale.map(VpssScale::new).transpose()?;

        Ok(Self {
            vdma_device,
            upscale,
            downscale,
            input_config: None,
        })
    }

    fn output_dims(&self, img: &RgbImage) -> (u32, u32) {
        if let Some(up) = self.upscale.as_ref() {
            dims_from(up.dims, img)
        } else {
            dims_from(self.downscale.dims, img)
        }
    }
    async fn maybe_reconfigure_pipeline(&mut self, image: &RgbImage) -> eyre::Result<()> {
        let (width, height) = (image.width(), image.height());
        self.input_config
            .take_if(|x| u32::from(x.width) != width && u32::from(x.height) != height);
        if self.input_config.is_some() {
            return Ok(());
        }
        let geometry = Geometry::new(width * FRAME_BPP, height)
            .map_err(|e| eyre!("Input image has unsupported geometry: {e}"))?;
        let input_config = FrameConfig {
            width: width
                .try_into()
                .wrap_err("Overflow in target image width")?,
            height: height
                .try_into()
                .wrap_err("Overflow in target image height")?,
            color: ColorFmt::Rgb,
        };

        self.vdma_device
            .set_geometry(geometry)
            .wrap_err("Failed to set new geometry")?;
        let downscale_config = self.downscale.cfg_for(image)?;
        let upscale_config = self
            .upscale
            .as_ref()
            .map(|x| x.cfg_for(image))
            .transpose()?;
        self.downscale
            .dev
            .apply_resize(input_config, downscale_config, self.downscale.nn)
            .wrap_err("Failed to reconfigure downscaler")?;
        if let Some(upscaler) = self.upscale.as_mut() {
            upscaler
                .dev
                .apply_resize(downscale_config, upscale_config.unwrap(), upscaler.nn)
                .wrap_err("Failed to reconfigure upscaler")?;
        }

        self.input_config = Some(input_config);
        Ok(())
    }
    pub async fn scale_batch(
        &mut self,
        images: impl IntoIterator<Item = RgbImage>,
    ) -> eyre::Result<Vec<RgbImage>> {
        let mut output = images.into_iter().collect::<Vec<_>>();

        for output in &mut output {
            self.maybe_reconfigure_pipeline(output).await?;
            self.vdma_device
                .write_frame(output.as_bytes())
                .await
                .wrap_err("Failed to write frame to output buffer")?;

            let buf = mem::take(output);
            let (out_width, out_height) = self.output_dims(&buf);
            let out_size = out_width * out_height * 3;

            let mut container = buf.into_raw();

            // Reuse the allocation of the input images for the output if we can.
            // We should be able to unless the user enters in a downscale/upscale size that is not
            container.resize(out_size as usize, 0);

            self.vdma_device
                .read_frame(&mut container)
                .await
                .wrap_err("Failed to read back frame")?;

            // We have guaranteed that this cannot be None since we just calculated the proper size
            let result = RgbImage::from_raw(out_width, out_height, container).unwrap();
            // Don't call Drop on a known empty Vec
            std::mem::forget(std::mem::replace(output, result));
        }

        Ok(output)
    }
}

/// Fit dimensions of an image into a given aspect ratio
pub(crate) fn fit_dims(in_width: u32, in_height: u32, x_cells: u32, y_cells: u32) -> (u32, u32) {
    // This is probably not the most efficient way to do this, but whatever
    let width = in_width.min(in_height * x_cells / y_cells);
    let height = in_height.min(in_width * y_cells / x_cells);
    // We need to make sure that each of the axes of the grid are actually divisible by the number of
    // grid squares on each axis
    (width - width % x_cells, height - height % y_cells)
}

pub(crate) async fn make_centered_grid(
    image: RgbImage,
    x_cells: u32,
    y_cells: u32,
    max_concurrency: u32,
) -> Vec<RgbImage> {
    // let image = smol::image.into_rgb8();
    // Fit the grid inside the image, rounding to fit the desired aspect ratio.
    let (full_grid_width, full_grid_height) =
        fit_dims(image.width(), image.height(), x_cells, y_cells);

    let x_offset = (image.width() - full_grid_width).div_ceil(2);
    let y_offset = (image.height() - full_grid_height).div_ceil(2);
    let grid_size = full_grid_width / x_cells;

    stream::iter(
        (0..y_cells)
            .flat_map(move |y| {
                (0..x_cells).map(move |x| (x_offset + x * grid_size, y_offset + y * grid_size))
            })
            .map(move |(x, y)| {
                // SAFETY: We make sure all of these tasks won't detatch and outlive the current function
                // since we know the inner function cannot panic
                let img = unsafe { detatch_lt(&image) };
                smol::unblock(move || {
                    imageops::crop_imm(img, x, y, grid_size, grid_size).to_image()
                })
            }),
    )
    .buffered(max_concurrency as _)
    .collect()
    .await
}

fn dims_from(dims: ScaleDims, img: &impl GenericImageView) -> (u32, u32) {
    match dims {
        ScaleDims::Absolute(dims) => (dims.width, dims.height),
        ScaleDims::Input => img.dimensions(),
    }
}
unsafe fn detatch_lt<'b, T>(x: &T) -> &'b T {
    unsafe { std::mem::transmute(x) }
}
#[cfg(test)]
mod test {

    use crate::scaler::fit_dims;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn test_fit(in_w in 0u32..65536, in_h in 0u32..65536, grid_w in 1u32..256, grid_h in 1u32..256) {
            if in_w <= grid_w || in_h <= grid_h {
                return Ok(());
            }
            let (w, h) = fit_dims(in_w, in_h, grid_w, grid_h);
            assert_eq!(w / grid_w, h / grid_h);
            assert!(w <= in_w);
            assert!(h <= in_h);
        }
        // #[test]
        // fn test_grid_crop(in_w in 0u32..65536, in_h in 0u32..65536, grid_w in 1u32..256, grid_h in 1u32..256) {
        //     if in_w <= grid_w || in_h <= grid_h {
        //         return Ok(());
        //     }
        //     let img = random_image(in_w, in_h);
        //     let images = smol::block_on(super::make_centered_grid(img, grid_w, grid_h, 8));
        //     assert_eq!(images.len(), (grid_w * grid_h) as usize)
        // }
    }
    // fn random_image(w: u32, h: u32) -> RgbImage {
    //     RgbImage::from_pixel(w, h, Rgb([4; 3]))
    // }
}
