// 矩形渲染着色器 - 顶点 + 片段
// 对应原 rect.v.glsl 和 rect.f.glsl
// 支持普通矩形, undercurl, dotted underline, dashed underline

struct RectUniforms {
    cell_width: f32,
    cell_height: f32,
    padding_x: f32,
    padding_y: f32,
    underline_position: f32,
    underline_thickness: f32,
    undercurl_position: f32,
    // 0 = normal, 1 = undercurl, 2 = dotted, 3 = dashed
    rect_kind: i32,
}

@group(0) @binding(0)
var<uniform> uniforms: RectUniforms;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.color = input.color;
    out.clip_position = vec4<f32>(input.position.x, input.position.y, 0.0, 1.0);
    return out;
}

const PI: f32 = 3.1415926538;

@fragment
fn fs_normal(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}

@fragment
fn fs_undercurl(input: VertexOutput) -> @location(0) vec4<f32> {
    let x = floor(((input.clip_position.x - uniforms.padding_x) % uniforms.cell_width));
    let y = floor(((input.clip_position.y - uniforms.padding_y) % uniforms.cell_height));

    // 使用 undercurl_position 作为振幅, 因为它是 descent 值的一半.
    // x 代表像素左边界, 加 0.5 使其计算像素中心的 undercurl 位置.
    let undercurl = uniforms.undercurl_position / 2.0
        * cos((x + 0.5) * 2.0 * PI / uniforms.cell_width)
        + uniforms.undercurl_position - 1.0;

    let half_extra = max((uniforms.underline_thickness - 1.0), 0.0) / 2.0;
    let undercurl_top = undercurl + half_extra;
    let undercurl_bottom = undercurl - half_extra;

    // 到曲线边界的距离, 在应用 AA 时始终为正.
    // 当 y - undercurl_top 和 undercurl_bottom - y 都为负时, 表示点在曲线内部, 使用 alpha 1.
    let dst = max(y - undercurl_top, max(undercurl_bottom - y, 0.0));

    // 简单的 AA: 通过 1/x^2 增强, 保持下划线粗细并足够醒目.
    let alpha = 1.0 - dst * dst;

    return vec4<f32>(input.color.rgb, alpha);
}

@fragment
fn fs_dotted(input: VertexOutput) -> @location(0) vec4<f32> {
    let x = floor(((input.clip_position.x - uniforms.padding_x) % uniforms.cell_width));
    let y = floor(((input.clip_position.y - uniforms.padding_y) % uniforms.cell_height));

    if uniforms.underline_thickness < 2.0 {
        // 点大小为单像素时的绘制
        var cell_even: f32 = 0.0;

        // 当 cell_width 为奇数时, 每两个 cell 反转模式以保持间距均匀
        if i32(uniforms.cell_width) % 2 != 0 {
            cell_even = (input.clip_position.x - uniforms.padding_x) / uniforms.cell_width % 2.0;
        }

        // 限制高度为单像素
        var alpha: f32 = 1.0 - abs(floor(uniforms.underline_position) - y);
        if i32(x) % 2 != i32(cell_even) {
            alpha = 0.0;
        }

        return vec4<f32>(input.color.rgb, alpha);
    } else {
        // 点大小较大时使用 AA 绘制
        let dot_number = floor(x / uniforms.underline_thickness);
        let radius = uniforms.underline_thickness / 2.0;
        let center_y = uniforms.underline_position - 1.0;

        let left_center = (dot_number - (dot_number % 2.0)) * uniforms.underline_thickness + radius;
        let right_center = left_center + 2.0 * uniforms.underline_thickness;

        let distance_left = sqrt(pow(x - left_center, 2.0) + pow(y - center_y, 2.0));
        let distance_right = sqrt(pow(x - right_center, 2.0) + pow(y - center_y, 2.0));

        let alpha = max(1.0 - (min(distance_left, distance_right) - radius), 0.0);
        return vec4<f32>(input.color.rgb, alpha);
    }
}

@fragment
fn fs_dashed(input: VertexOutput) -> @location(0) vec4<f32> {
    let x = floor(((input.clip_position.x - uniforms.padding_x) % uniforms.cell_width));

    // 相邻 cell 的虚线相互连接, 所以虚线长度取期望总长度的一半
    let half_dash_len = floor(uniforms.cell_width / 4.0 + 0.5);

    var alpha: f32 = 1.0;
    // 检查 x 坐标是否在间隙区域
    if x > half_dash_len - 1.0 && x < uniforms.cell_width - half_dash_len {
        alpha = 0.0;
    }

    return vec4<f32>(input.color.rgb, alpha);
}
