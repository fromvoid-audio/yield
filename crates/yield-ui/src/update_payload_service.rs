//! Private compiled-context registration for the actual retained payload owner.
use std::sync::Arc;
use yield_core::{ReleaseBuildIdentity, ReleaseChannel};
use yield_platform::{
    payload_selection::PayloadStagingService,
    update_catalog::{Channel, Platform},
};

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct InstalledPayload {
    pub(crate) full_version: String,
    pub(crate) number: u64,
    pub(crate) commit: [u8; 20],
    pub(crate) platform: Platform,
}
pub(crate) struct PayloadContext {
    pub(crate) installed: InstalledPayload,
    pub(crate) owner: PayloadStagingService,
}
impl Drop for PayloadContext {
    fn drop(&mut self) {
        self.owner.retire();
    }
}
#[derive(Clone)]
pub(crate) struct UpdatePayloadService {
    pub(crate) context: Arc<PayloadContext>,
    pub(crate) channel: Channel,
}
impl gpui::Global for UpdatePayloadService {}
impl UpdatePayloadService {
    fn compiled() -> Option<Self> {
        let identity = ReleaseBuildIdentity::compiled().ok()?;
        let platform = match identity.target() {
            "aarch64-apple-darwin" => Platform::MacArm64,
            "x86_64-apple-darwin" => Platform::MacX64,
            "x86_64-pc-windows-msvc" => Platform::WindowsX64,
            _ => return None,
        };
        let channel = match identity.channel() {
            ReleaseChannel::Stable => Channel::Stable,
            ReleaseChannel::Rc => Channel::Rc,
            ReleaseChannel::Nightly => Channel::Nightly,
        };
        Some(Self {
            context: Arc::new(PayloadContext {
                installed: InstalledPayload {
                    full_version: identity.version().into(),
                    number: identity.build_number(),
                    commit: identity.commit(),
                    platform,
                },
                owner: PayloadStagingService::compiled()?,
            }),
            channel,
        })
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.context, &other.context)
            && self.channel == other.channel
            && self.context.installed == other.context.installed
    }
}
pub(crate) fn register(cx: &mut gpui::App) {
    replace(cx, UpdatePayloadService::compiled());
}
fn replace(cx: &mut gpui::App, service: Option<UpdatePayloadService>) {
    if let Some(old) = cx.try_global::<UpdatePayloadService>() {
        old.context.owner.retire();
    }
    match service {
        Some(service) => cx.set_global(service),
        None => {
            if cx.try_global::<UpdatePayloadService>().is_some() {
                cx.remove_global::<UpdatePayloadService>();
            }
        }
    }
}
#[cfg(test)]
pub(crate) mod tests;
