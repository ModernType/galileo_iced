// Needed to convert between galileo's and iced color formats

@group(0) @binding(0) var map_texture: texture_2d<f32>;
@group(0) @binding(1) var map_sampler: sampler;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // (0,0), (2,0), (0,2) in UV space -> a triangle that covers [0,1]^2.
    let uv = vec2<f32>(
        f32((index << 1u) & 2u),
        f32(index & 2u),
    );

    var out: VertexOutput;
    out.uv = uv;
    out.clip_position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);

    return out;
}

@fragment
fn fs_passthrough(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(map_texture, map_sampler, in.uv);
}

fn srgb_to_linear(color: vec3<f32>) -> vec3<f32> {
    let cutoff = color <= vec3<f32>(0.04045);
    let low = color / 12.92;
    let high = pow((color + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));

    return select(high, low, cutoff);
}

@fragment
fn fs_srgb(in: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(map_texture, map_sampler, in.uv);

    return vec4<f32>(srgb_to_linear(color.rgb), color.a);
}
