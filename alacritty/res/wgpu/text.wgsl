// 文本渲染着色器 - 顶点 + 片段
// 对应原 glsl3/text.v.glsl 和 glsl3/text.f.glsl

struct Uniforms {
    // projection: offset_x, offset_y, scale_x, scale_y
    projection: vec4<f32>,
    // cell_dim: cell_width, cell_height
    cell_dim: vec2<f32>,
    // rendering_pass: 0 = background, 1 = text
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
    @location(0) grid_coords: vec2<u32>,    // col, row (u16 x2 packed as u32 x2)
    @location(1) glyph: vec4<i32>,          // left, top, width, height (i16 x4 packed as i32 x4)
    @location(2) uv: vec4<f32>,             // uv_left, uv_bot, uv_width, uv_height
    @location(3) text_color: vec4<u32>,     // r, g, b, cell_flags (u8 x4 packed as u32 x4)
    @location(4) bg_color: vec4<u32>,       // bg_r, bg_g, bg_b, bg_a (u8 x4 packed as u32 x4)
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) @interpolate(flat) fg: vec4<f32>,
    @location(2) @interpolate(flat) bg: vec4<f32>,
}

const WIDE_CHAR: u32 = 2u;
const COLORED: u32 = 1u;

@vertex
fn vs_main(
    input: VertexInput,
    @builtin(vertex_index) vertex_index: u32,
) -> VertexOutput {
    var out: VertexOutput;

    let projection_offset = uniforms.projection.xy;
    let projection_scale = uniforms.projection.zw;

    // 计算顶点角落位置 (0 或 1)
    // 顶点顺序: 0=右上, 1=右下, 2=左下, 3=左上  (用于 0,1,3 / 1,2,3 两个三角形)
    var pos: vec2<f32>;
    if vertex_index == 0u || vertex_index == 1u {
        pos.x = 1.0;
    } else {
        pos.x = 0.0;
    }
    if vertex_index == 0u || vertex_index == 3u {
        pos.y = 0.0;
    } else {
        pos.y = 1.0;
    }

    // 从左上角计算 cell 位置
    let cell_position = uniforms.cell_dim * vec2<f32>(f32(input.grid_coords.x), f32(input.grid_coords.y));

    let fg_rgb = vec3<f32>(f32(input.text_color.x), f32(input.text_color.y), f32(input.text_color.z)) / 255.0;
    let cell_flags = input.text_color.w;
    out.bg = vec4<f32>(
        f32(input.bg_color.x) / 255.0,
        f32(input.bg_color.y) / 255.0,
        f32(input.bg_color.z) / 255.0,
        f32(input.bg_color.w) / 255.0,
    );

    var occupied_cells: f32 = 1.0;
    var flags_remainder = cell_flags;
    if cell_flags >= WIDE_CHAR {
        occupied_cells = 2.0;
        flags_remainder = cell_flags - WIDE_CHAR;
    }
    out.fg = vec4<f32>(fg_rgb, f32(flags_remainder));

    if uniforms.rendering_pass == 0 {
        // 背景渲染 pass
        var background_dim = uniforms.cell_dim;
        background_dim.x *= occupied_cells;

        let final_position = cell_position + background_dim * pos;
        out.position = vec4<f32>(projection_offset + projection_scale * final_position, 0.0, 1.0);
        out.tex_coords = vec2<f32>(0.0, 0.0);
    } else {
        // 文字渲染 pass
        let glyph_size = vec2<f32>(f32(input.glyph.z), f32(input.glyph.w));
        var glyph_offset = vec2<f32>(f32(input.glyph.x), f32(input.glyph.y));
        glyph_offset.y = uniforms.cell_dim.y - glyph_offset.y;

        let final_position = cell_position + glyph_size * pos + glyph_offset;
        out.position = vec4<f32>(projection_offset + projection_scale * final_position, 0.0, 1.0);

        let uv_offset = input.uv.xy;
        let uv_size = input.uv.zw;
        out.tex_coords = uv_offset + pos * uv_size;
    }

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
    let colored = input.fg.a;

    if u32(colored) == COLORED {
        // 彩色字形 (例如 emoji)
        var glyph_color = textureSample(glyph_texture, glyph_sampler, input.tex_coords);
        // 直接输出带 alpha 的颜色
        return glyph_color;
    } else {
        // 普通文本字形 - 使用纹理作为 alpha mask
        let mask = textureSample(glyph_texture, glyph_sampler, input.tex_coords);
        let alpha = mask.r;
        if alpha < 0.001 {
            discard;
        }
        return vec4<f32>(input.fg.rgb * alpha, alpha);
    }
}
