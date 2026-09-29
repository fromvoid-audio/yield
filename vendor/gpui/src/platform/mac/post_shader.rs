//! One optional WGSL fragment pass over the composed Metal window image.

use metal::{MTLPixelFormat, MTLTextureUsage};
use naga::{
    ResourceBinding,
    back::msl::{BindSamplerTarget, BindTarget, EntryPointResources, Options, PipelineOptions},
    valid::{Capabilities, ValidationFlags, Validator},
};

const VERTEX_SOURCE: &str = r#"
struct PostVertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn post_vertex(@builtin(vertex_index) index: u32) -> PostVertex {
    var out: PostVertex;
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let position = positions[index];
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.uv = vec2<f32>((position.x + 1.0) * 0.5, (1.0 - position.y) * 0.5);
    return out;
}
"#;

pub(super) struct PostShader {
    pub pipeline: metal::RenderPipelineState,
    pub sampler: metal::SamplerState,
}

impl PostShader {
    pub fn compile(device: &metal::DeviceRef, source: &str) -> Result<Self, String> {
        if source.len() > 64 * 1024 {
            return Err(String::from("WGSL post shader exceeds the 64 KiB source limit"));
        }
        let module = naga::front::wgsl::parse_str(&format!("{VERTEX_SOURCE}\n{source}"))
            .map_err(|error| format!("WGSL parse error: {error}"))?;
        if module.entry_points.len() != 2
            || !module.entry_points.iter().any(|entry| {
                entry.stage == naga::ShaderStage::Fragment && entry.name == "post_fragment"
            })
        {
            return Err(String::from(
                "post shader must provide one @fragment fn post_fragment",
            ));
        }
        let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
            .validate(&module)
            .map_err(|error| format!("WGSL validation error: {error}"))?;
        let mut resources = EntryPointResources::default();
        resources.resources.insert(
            ResourceBinding { group: 0, binding: 0 },
            BindTarget {
                texture: Some(0),
                ..Default::default()
            },
        );
        resources.resources.insert(
            ResourceBinding { group: 0, binding: 1 },
            BindTarget {
                sampler: Some(BindSamplerTarget::Resource(0)),
                ..Default::default()
            },
        );
        let mut options = Options {
            lang_version: (2, 3),
            fake_missing_bindings: false,
            ..Default::default()
        };
        options
            .per_entry_point_map
            .insert(String::from("post_fragment"), resources);
        let (metal_source, translated) =
            naga::back::msl::write_string(&module, &info, &options, &PipelineOptions::default())
                .map_err(|error| format!("WGSL Metal translation error: {error}"))?;
        let library = device
            .new_library_with_source(&metal_source, &metal::CompileOptions::new())
            .map_err(|error| format!("Metal shader compilation failed: {error}"))?;
        let entry_name = |name: &str| -> Result<String, String> {
            let index = module
                .entry_points
                .iter()
                .position(|entry| entry.name == name)
                .ok_or_else(|| format!("missing {name} entry point"))?;
            translated.entry_point_names[index]
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| format!("Metal entry point translation failed: {error}"))
        };
        let vertex = library
            .get_function(&entry_name("post_vertex")?, None)
            .map_err(|error| format!("Metal vertex lookup failed: {error}"))?;
        let fragment = library
            .get_function(&entry_name("post_fragment")?, None)
            .map_err(|error| format!("Metal fragment lookup failed: {error}"))?;
        let descriptor = metal::RenderPipelineDescriptor::new();
        descriptor.set_label("gpui visual WGSL post pass");
        descriptor.set_vertex_function(Some(vertex.as_ref()));
        descriptor.set_fragment_function(Some(fragment.as_ref()));
        descriptor
            .color_attachments()
            .object_at(0)
            .ok_or_else(|| String::from("missing post-pass color attachment"))?
            .set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        let pipeline = device
            .new_render_pipeline_state(&descriptor)
            .map_err(|error| format!("Metal post-pass pipeline failed: {error}"))?;
        let sampler_descriptor = metal::SamplerDescriptor::new();
        sampler_descriptor.set_min_filter(metal::MTLSamplerMinMagFilter::Linear);
        sampler_descriptor.set_mag_filter(metal::MTLSamplerMinMagFilter::Linear);
        sampler_descriptor.set_address_mode_s(metal::MTLSamplerAddressMode::ClampToEdge);
        sampler_descriptor.set_address_mode_t(metal::MTLSamplerAddressMode::ClampToEdge);
        let sampler = device.new_sampler(&sampler_descriptor);
        Ok(Self { pipeline, sampler })
    }
}

pub(super) fn new_target(
    device: &metal::DeviceRef,
    width: u64,
    height: u64,
) -> metal::Texture {
    let descriptor = metal::TextureDescriptor::new();
    descriptor.set_texture_type(metal::MTLTextureType::D2);
    descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    descriptor.set_width(width);
    descriptor.set_height(height);
    descriptor.set_usage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
    descriptor.set_storage_mode(metal::MTLStorageMode::Private);
    device.new_texture(&descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_valid_effect_and_rejects_invalid_wgsl() {
        let device = metal::Device::system_default().expect("Metal device");
        let valid = r#"
@group(0) @binding(0) var screen: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;
@fragment
fn post_fragment(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(screen, screen_sampler, uv);
}
"#;
        PostShader::compile(&device, valid).expect("valid WGSL compiles to a Metal pipeline");
        assert!(PostShader::compile(&device, "not wgsl").is_err());
    }
}
