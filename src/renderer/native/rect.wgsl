// Instanced rectangles and text decorations.

struct FrameUniforms {
    projection: vec4<f32>,
    cell_dim: vec2<f32>,
    padding: vec2<f32>,
    underline_position: f32,
    underline_thickness: f32,
    undercurl_position: f32,
    _pad: f32,
}

@group(0) @binding(0)
var<uniform> uniforms: FrameUniforms;

struct VertexInput {
    // x, y, width, height in content-viewport pixels.
    @location(0) position_size: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) kind: u32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    @location(1) @interpolate(flat) kind: u32,
}

fn vertex_corner(vertex_index: u32) -> vec2<f32> {
    var corner_index: u32;
    switch vertex_index {
        case 0u: { corner_index = 0u; }
        case 1u: { corner_index = 1u; }
        case 2u: { corner_index = 3u; }
        case 3u: { corner_index = 1u; }
        case 4u: { corner_index = 2u; }
        default: { corner_index = 3u; }
    }

    return vec2<f32>(
        select(0.0, 1.0, corner_index == 0u || corner_index == 1u),
        select(1.0, 0.0, corner_index == 0u || corner_index == 3u),
    );
}

@vertex
fn vs_main(
    input: VertexInput,
    @builtin(vertex_index) vertex_index: u32,
) -> VertexOutput {
    var out: VertexOutput;
    let corner = vertex_corner(vertex_index);
    let position = input.position_size.xy + input.position_size.zw * corner;
    out.clip_position = vec4<f32>(
        uniforms.projection.xy + uniforms.projection.zw * position,
        0.0,
        1.0,
    );
    out.color = input.color;
    out.kind = input.kind;
    return out;
}

const PI: f32 = 3.1415926538;

fn undercurl(input: VertexOutput) -> vec4<f32> {
    let x = floor((input.clip_position.x - uniforms.padding.x) % uniforms.cell_dim.x);
    let y = floor((input.clip_position.y - uniforms.padding.y) % uniforms.cell_dim.y);

    let center = uniforms.undercurl_position / 2.0
        * cos((x + 0.5) * 2.0 * PI / uniforms.cell_dim.x)
        + uniforms.undercurl_position - 1.0;
    let half_extra = max(uniforms.underline_thickness - 1.0, 0.0) / 2.0;
    let top = center + half_extra;
    let bottom = center - half_extra;
    let distance = max(y - top, max(bottom - y, 0.0));
    let alpha = clamp(1.0 - distance * distance, 0.0, 1.0) * input.color.a;

    return vec4<f32>(input.color.rgb, alpha);
}

fn dotted(input: VertexOutput) -> vec4<f32> {
    let x = floor((input.clip_position.x - uniforms.padding.x) % uniforms.cell_dim.x);
    let y = floor((input.clip_position.y - uniforms.padding.y) % uniforms.cell_dim.y);

    if uniforms.underline_thickness < 2.0 {
        var cell_even: f32 = 0.0;
        if i32(uniforms.cell_dim.x) % 2 != 0 {
            cell_even = (input.clip_position.x - uniforms.padding.x) / uniforms.cell_dim.x % 2.0;
        }

        var alpha = 1.0 - abs(floor(uniforms.underline_position) - y);
        if i32(x) % 2 != i32(cell_even) {
            alpha = 0.0;
        }
        alpha = clamp(alpha, 0.0, 1.0) * input.color.a;
        return vec4<f32>(input.color.rgb, alpha);
    }

    let dot_number = floor(x / uniforms.underline_thickness);
    let radius = uniforms.underline_thickness / 2.0;
    let center_y = uniforms.underline_position - 1.0;
    let left_center = (dot_number - (dot_number % 2.0)) * uniforms.underline_thickness + radius;
    let right_center = left_center + 2.0 * uniforms.underline_thickness;
    let dx_left = x - left_center;
    let dx_right = x - right_center;
    let dy = y - center_y;
    let distance_left = sqrt(dx_left * dx_left + dy * dy);
    let distance_right = sqrt(dx_right * dx_right + dy * dy);
    let alpha = clamp(
        max(1.0 - (min(distance_left, distance_right) - radius), 0.0),
        0.0,
        1.0,
    ) * input.color.a;

    return vec4<f32>(input.color.rgb, alpha);
}

fn dashed(input: VertexOutput) -> vec4<f32> {
    let x = floor((input.clip_position.x - uniforms.padding.x) % uniforms.cell_dim.x);
    let half_dash_len = floor(uniforms.cell_dim.x / 4.0 + 0.5);

    var alpha = 1.0;
    if x > half_dash_len - 1.0 && x < uniforms.cell_dim.x - half_dash_len {
        alpha = 0.0;
    }
    alpha = clamp(alpha, 0.0, 1.0) * input.color.a;

    return vec4<f32>(input.color.rgb, alpha);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    switch input.kind {
        case 1u: { return undercurl(input); }
        case 2u: { return dotted(input); }
        case 3u: { return dashed(input); }
        default: { return input.color; }
    }
}
