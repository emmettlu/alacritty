// 文本渲染着色器 - 顶点 + 片段
// 背景和文字使用不同 vertex entry, 避免每个顶点根据 pass 分支.

struct Uniforms {
    // projection: offset_x, offset_y, scale_x, scale_y
    projection: vec4<f32>,
    // cell_dim: cell_width, cell_height
    cell_dim: vec2<f32>,
    // 保留 padding, 让 Rust 侧 uniform 结构不需要变化.
    rendering_pass: i32,
    _pad: i32,
}

@group(0) @binding(0)
var<uniform> uniforms: Uniforms;

@group(1) @binding(0)
var glyph_texture: texture_2d<f32>;
@group(1) @binding(1)
var glyph_sampler: sampler;

struct VertexInput {
    // 逐实例数据, 通过 instance buffer 传入
    @location(0) grid_coords: vec2<u32>,
    @location(1) glyph: vec4<i32>,
    @location(2) uv: vec4<f32>,
    @location(3) text_color: vec4<u32>,
    @location(4) bg_color: vec4<u32>,
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) @interpolate(flat) fg: vec3<f32>,
    @location(2) @interpolate(flat) bg: vec4<f32>,
    @location(3) @interpolate(flat) cell_flags: u32,
}

const WIDE_CHAR: u32 = 2u;
const COLORED: u32 = 1u;

fn vertex_corner(vertex_index: u32) -> vec2<f32> {
    // 顶点顺序: 0=右上, 1=右下, 2=左下, 3=左上  (用于 0,1,3 / 1,2,3 两个三角形)
    return vec2<f32>(
        select(0.0, 1.0, vertex_index == 0u || vertex_index == 1u),
        select(1.0, 0.0, vertex_index == 0u || vertex_index == 3u),
    );
}

fn fill_colors(input: VertexInput, out: ptr<function, VertexOutput>) {
    (*out).fg = vec3<f32>(
        f32(input.text_color.x),
        f32(input.text_color.y),
        f32(input.text_color.z),
    ) / 255.0;
    (*out).cell_flags = input.text_color.w;
    (*out).bg = vec4<f32>(
        f32(input.bg_color.x),
        f32(input.bg_color.y),
        f32(input.bg_color.z),
        f32(input.bg_color.w),
    ) / 255.0;
}

@vertex
fn vs_bg(
    input: VertexInput,
    @builtin(vertex_index) vertex_index: u32,
) -> VertexOutput {
    var out: VertexOutput;
    fill_colors(input, &out);

    let pos = vertex_corner(vertex_index);
    let cell_position = uniforms.cell_dim * vec2<f32>(f32(input.grid_coords.x), f32(input.grid_coords.y));

    var background_dim = uniforms.cell_dim;
    if (input.text_color.w & WIDE_CHAR) != 0u {
        background_dim.x *= 2.0;
    }

    let final_position = cell_position + background_dim * pos;
    out.position = vec4<f32>(uniforms.projection.xy + uniforms.projection.zw * final_position, 0.0, 1.0);
    out.tex_coords = vec2<f32>(0.0, 0.0);
    return out;
}

@vertex
fn vs_text(
    input: VertexInput,
    @builtin(vertex_index) vertex_index: u32,
) -> VertexOutput {
    var out: VertexOutput;
    fill_colors(input, &out);

    let pos = vertex_corner(vertex_index);
    let cell_position = uniforms.cell_dim * vec2<f32>(f32(input.grid_coords.x), f32(input.grid_coords.y));

    let glyph_size = vec2<f32>(f32(input.glyph.z), f32(input.glyph.w));
    var glyph_offset = vec2<f32>(f32(input.glyph.x), f32(input.glyph.y));
    glyph_offset.y = uniforms.cell_dim.y - glyph_offset.y;

    let final_position = cell_position + glyph_size * pos + glyph_offset;
    out.position = vec4<f32>(uniforms.projection.xy + uniforms.projection.zw * final_position, 0.0, 1.0);

    out.tex_coords = input.uv.xy + pos * input.uv.zw;
    return out;
}

// 背景 pass 的片段着色器入口
@fragment
fn fs_bg(input: VertexOutput) -> @location(0) vec4<f32> {
    if input.bg.a == 0.0 {
        discard;
    }

    // 预乘 alpha
    return vec4<f32>(input.bg.rgb * input.bg.a, input.bg.a);
}

// 文字 pass 的片段着色器入口
@fragment
fn fs_text(input: VertexOutput) -> @location(0) vec4<f32> {
    if (input.cell_flags & COLORED) != 0u {
        // 彩色字形 (例如 emoji)
        return textureSample(glyph_texture, glyph_sampler, input.tex_coords);
    }

    // 普通文本字形 - 使用纹理作为 alpha mask
    let mask = textureSample(glyph_texture, glyph_sampler, input.tex_coords);
    let alpha = mask.r;
    if alpha < 0.001 {
        discard;
    }

    return vec4<f32>(input.fg * alpha, alpha);
}
