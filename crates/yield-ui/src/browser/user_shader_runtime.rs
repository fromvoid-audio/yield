//! Bounded authorization, ABI checking, and single-flight preparation for custom WGSL.

use std::sync::atomic::{AtomicBool, Ordering};

use naga::{
    AddressSpace, ImageClass, ImageDimension, ScalarKind, ShaderStage, TypeInner,
    valid::{Capabilities, ValidationFlags, Validator},
};

static PREPARATION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Process-wide permit retained through synchronous native pipeline preparation.
pub(super) struct PreparationPermit;

impl PreparationPermit {
    pub(super) fn try_acquire() -> Option<Self> {
        PREPARATION_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for PreparationPermit {
    fn drop(&mut self) {
        PREPARATION_ACTIVE.store(false, Ordering::Release);
    }
}

/// Parse/type-check a user fragment and enforce the bounded single-pass ABI v1.
pub(super) fn validate_user_shader_abi(source: &str) -> Result<(), String> {
    let module =
        naga::front::wgsl::parse_str(source).map_err(|error| error.emit_to_string(source))?;
    if module.entry_points.len() != 1
        || module.entry_points[0].stage != ShaderStage::Fragment
        || module.entry_points[0].name != "post_fragment"
    {
        return Err(String::from(
            "custom WGSL must declare only the post_fragment fragment entry point",
        ));
    }

    for (_, global) in module.global_variables.iter() {
        match global.space {
            AddressSpace::Private => {
                if global.binding.is_some() {
                    return Err(String::from("private WGSL variables cannot have bindings"));
                }
            }
            AddressSpace::Handle => {
                let Some(binding) = global.binding else {
                    return Err(String::from("unbound WGSL resources are not supported"));
                };
                let ty = &module.types[global.ty].inner;
                let valid = match (binding.group, binding.binding, ty) {
                    (
                        0,
                        0,
                        TypeInner::Image {
                            dim: ImageDimension::D2,
                            arrayed: false,
                            class:
                                ImageClass::Sampled {
                                    kind: ScalarKind::Float,
                                    multi: false,
                                },
                        },
                    ) => true,
                    (0, 1, TypeInner::Sampler { comparison: false }) => true,
                    _ => false,
                };
                if !valid {
                    return Err(String::from(
                        "custom WGSL supports only group 0 binding 0 texture_2d<f32> and binding 1 sampler",
                    ));
                }
            }
            _ => {
                return Err(String::from(
                    "custom WGSL cannot declare uniform, storage, workgroup, or push-constant resources",
                ));
            }
        }
    }

    Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .map_err(|error| format!("custom WGSL validation failed: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@fragment
fn post_fragment(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv);
}
"#;

    #[test]
    fn custom_shader_abi_accepts_only_single_pass_screen_texture_and_sampler() {
        validate_user_shader_abi(VALID).expect("valid ABI-v1 fragment");

        for invalid in [
            VALID.replace("@binding(1)", "@binding(2)"),
            VALID.replace("@group(0) @binding(1)", "@group(1) @binding(1)"),
            VALID.replace("texture_2d<f32>", "texture_2d_array<f32>"),
            VALID.replace("sampler;", "sampler_comparison;"),
            VALID.replace(
                "@fragment\nfn post_fragment",
                "@compute @workgroup_size(1)\nfn post_fragment",
            ),
            format!("{VALID}\n@group(0) @binding(3) var<uniform> settings: vec4<f32>;"),
            format!("{VALID}\nvar<workgroup> shared: u32;"),
            VALID.replace("@group(0) @binding(1) ", ""),
            format!(
                "{VALID}\n@fragment fn second(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {{ return textureSample(source_texture, source_sampler, uv); }}"
            ),
        ] {
            assert!(
                validate_user_shader_abi(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn synchronous_prepare_keeps_permit_after_request_invalidation() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let permit = PreparationPermit::try_acquire().expect("first permit");
        assert!(PreparationPermit::try_acquire().is_none());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let preparation = thread::spawn(move || {
            let _permit = permit;
            started_tx.send(()).expect("signal preparation started");
            release_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("wait for synchronous preparation release");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("preparation entered synchronous work");
        // An invalidated window request cannot release the process permit while
        // the non-cancellable synchronous prepare still owns it.
        assert!(PreparationPermit::try_acquire().is_none());
        release_tx.send(()).expect("release preparation");
        preparation.join().expect("preparation drains safely");
        assert!(PreparationPermit::try_acquire().is_some());
    }
}
