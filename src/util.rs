use generic_camera::Sleep;

#[macro_export]
macro_rules! display_prop {
    ($e:expr) => {
        match $e {
            _ if false => &"" as &dyn std::fmt::Display,
            PropertyValue::Bool(b) => b,
            PropertyValue::Duration(d) => &DurationString::from(*d),
            PropertyValue::EnumStr(s) => s,
            PropertyValue::Float(f) => f,
            PropertyValue::Int(i) => i,
            PropertyValue::Unsigned(u) => u,
            PropertyValue::PixelFmt(f) => &format!("{f:?}"),
            PropertyValue::Command => &"<command>",
            _ => &"<unknown value>",
        }
    };
}

pub struct SmolSleep;
impl Sleep for SmolSleep {
    #[allow(
        clippy::manual_async_fn,
        reason = "this cannot be simplified to use since the generated future would\
        be + '_ despite never touching `self` and therefore not 'static.\
        Clippy issue #14372"
    )]
    fn sleep(
        &self,
        duration: std::time::Duration,
    ) -> impl Future<Output = ()> + Send + Sync + 'static {
        async move {
            smol::Timer::after(duration).await;
        }
    }
}
