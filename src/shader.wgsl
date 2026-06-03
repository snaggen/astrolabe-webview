// Simple RGBA texture renderer.
// Uses the fullscreen-triangle trick combined with set_viewport to confine
// rendering to the widget bounds — no uniform needed for position.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;

@vertex
fn vs_main(@builtin(vertex_index) in_vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    // Three-vertex triangle that covers the entire clip space.
    // set_viewport in the render pass clips it to the widget bounds.
    out.uv.x = select(0.0, 2.0, in_vertex_index == 1u);
    out.uv.y = select(0.0, 2.0, in_vertex_index == 2u);
    out.position = vec4<f32>(
        out.uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0),
        1.0,
        1.0,
    );
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(tex, s, in.uv);
}
