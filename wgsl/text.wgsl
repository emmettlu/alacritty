// 文本渲染 shader.
//
// CPU 侧每个实例对应一个待绘制的 cell/glyph. 同一份实例数据会被背景 pass
// (`vs_bg`/`fs_bg`) 和字形 pass (`vs_text`/`fs_text`) 复用.

struct FrameUniforms {
    // projection.xy 是内容区域左上角对应的 clip-space 偏移.
    // projection.zw 把内容区域内的像素坐标缩放到 clip-space.
    projection: vec4<f32>,

    // 单个终端 cell 的像素尺寸.
    cell_dim: vec2<f32>,
    padding: vec2<f32>,
    underline_position: f32,
    underline_thickness: f32,
    undercurl_position: f32,
    _pad: f32,
}

@group(0) @binding(0)
var<uniform> uniforms: FrameUniforms;

// 字形 atlas. 背景 pass 不采样它, 但为了复用 pipeline layout 仍绑定同一组资源.
@group(1) @binding(0)
var glyph_texture: texture_2d<f32>;
@group(1) @binding(1)
var glyph_sampler: sampler;

struct VertexInput {
    // cell 在终端网格中的列/行.
    @location(0) grid_coords: vec2<u32>,

    // glyph.x/y 是 glyph 相对 cell 的像素偏移, glyph.z/w 是 glyph 像素尺寸.
    @location(1) glyph: vec4<i32>,

    // atlas UV: left, top, width, height.
    @location(2) uv: vec4<f32>,

    // RGB 是已转换到线性空间的前景色字节, w 是 cell flags.
    @location(3) text_color: vec4<u32>,

    // 背景色和 alpha, 由顶点格式以 unorm 传入.
    @location(4) bg_color: vec4<f32>,
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
    // 一个实例直接绘制 6 个顶点组成两个三角形.
    // corner_index 的含义: 0=右上, 1=右下, 2=左下, 3=左上.
    // 顺序等价于四点 quad 的索引序列 0,1,3 / 1,2,3.
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

fn fill_colors(input: VertexInput, out: ptr<function, VertexOutput>) {
    // text_color 是 Uint8x4 传入的整数, 这里转成 0..1 的线性 RGB.
    (*out).fg = vec3<f32>(
        f32(input.text_color.x),
        f32(input.text_color.y),
        f32(input.text_color.z),
    ) / 255.0;
    (*out).cell_flags = input.text_color.w;
    (*out).bg = input.bg_color;
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

    // 背景覆盖整个 cell. 宽字符的可见 cell 横向占两个普通 cell.
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

    // CPU 传入的 glyph y 偏移以基线/上边距度量为基础, 这里转换为内容区域
    // 左上角向下增长的像素坐标.
    glyph_offset.y = uniforms.cell_dim.y - glyph_offset.y;

    let final_position = cell_position + glyph_size * pos + glyph_offset;
    out.position = vec4<f32>(uniforms.projection.xy + uniforms.projection.zw * final_position, 0.0, 1.0);

    out.tex_coords = input.uv.xy + pos * input.uv.zw;
    return out;
}

@fragment
fn fs_bg(input: VertexOutput) -> @location(0) vec4<f32> {
    if input.bg.a == 0.0 {
        discard;
    }

    // 背景 pass 使用预乘 alpha blend, 输出也必须是预乘颜色.
    return vec4<f32>(input.bg.rgb * input.bg.a, input.bg.a);
}

@fragment
fn fs_text(input: VertexOutput) -> @location(0) vec4<f32> {
    if (input.cell_flags & COLORED) != 0u {
        // 彩色 glyph, 例如 emoji, atlas 中已经存放最终 RGBA.
        return textureSample(glyph_texture, glyph_sampler, input.tex_coords);
    }

    // 普通 glyph 只使用 atlas alpha 作为 coverage mask, RGB 来自 cell 前景色.
    let mask = textureSample(glyph_texture, glyph_sampler, input.tex_coords);
    let alpha = mask.a;
    if alpha < 0.001 {
        discard;
    }

    return vec4<f32>(input.fg, alpha);
}
