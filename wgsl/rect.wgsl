// 矩形和文本装饰线 shader.
//
// CPU 侧已经把矩形展开为 clip-space 顶点. fragment 阶段的
// `@builtin(position)` 会变成当前片元的 framebuffer 像素坐标, 因此
// undercurl/dotted/dashed 可以用它计算 cell 内局部位置.

struct RectUniforms {
    // cell 尺寸, 用于把片元坐标映射到当前 cell 内.
    cell_width: f32,
    cell_height: f32,

    // 内容区域 padding, 用于把 framebuffer 坐标转换成终端内容坐标.
    padding_x: f32,
    padding_y: f32,

    // 下划线/装饰线的字体度量, 单位为像素.
    underline_position: f32,
    underline_thickness: f32,
    undercurl_position: f32,
}

@group(0) @binding(0)
var<uniform> uniforms: RectUniforms;

struct VertexInput {
    // CPU 已经计算好的 clip-space 坐标.
    @location(0) position: vec2<f32>,

    // 已转换到线性空间的 RGBA.
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
    // 普通矩形已经由 CPU 给出最终位置和颜色, 直接输出.
    return input.color;
}

@fragment
fn fs_undercurl(input: VertexOutput) -> @location(0) vec4<f32> {
    // 片元在当前 cell 内的像素位置.
    let x = floor(((input.clip_position.x - uniforms.padding_x) % uniforms.cell_width));
    let y = floor(((input.clip_position.y - uniforms.padding_y) % uniforms.cell_height));

    // 用余弦波生成 undercurl 中心线. undercurl_position 由字体 descent 推导,
    // 同时决定波形的垂直位置和幅度.
    let undercurl = uniforms.undercurl_position / 2.0
        * cos((x + 0.5) * 2.0 * PI / uniforms.cell_width)
        + uniforms.undercurl_position - 1.0;

    // thickness 大于 1px 时扩展曲线覆盖范围.
    let half_extra = max((uniforms.underline_thickness - 1.0), 0.0) / 2.0;
    let undercurl_top = undercurl + half_extra;
    let undercurl_bottom = undercurl - half_extra;

    // dst 为片元到曲线覆盖范围外边界的距离. 覆盖范围内为 0.
    let dst = max(y - undercurl_top, max(undercurl_bottom - y, 0.0));

    // 简单 AA, 并保留 CPU 传入的整体 alpha.
    let alpha = clamp(1.0 - dst * dst, 0.0, 1.0) * input.color.a;

    return vec4<f32>(input.color.rgb, alpha);
}

@fragment
fn fs_dotted(input: VertexOutput) -> @location(0) vec4<f32> {
    // 片元在当前 cell 内的像素位置.
    let x = floor(((input.clip_position.x - uniforms.padding_x) % uniforms.cell_width));
    let y = floor(((input.clip_position.y - uniforms.padding_y) % uniforms.cell_height));

    if uniforms.underline_thickness < 2.0 {
        // 细 dotted underline 使用单像素点阵. cell 宽度为奇数时, 相邻 cell
        // 需要翻转奇偶性, 避免点间距在 cell 边界处不均匀.
        var cell_even: f32 = 0.0;

        if i32(uniforms.cell_width) % 2 != 0 {
            cell_even = (input.clip_position.x - uniforms.padding_x) / uniforms.cell_width % 2.0;
        }

        var alpha: f32 = 1.0 - abs(floor(uniforms.underline_position) - y);
        if i32(x) % 2 != i32(cell_even) {
            alpha = 0.0;
        }
        alpha = clamp(alpha, 0.0, 1.0) * input.color.a;

        return vec4<f32>(input.color.rgb, alpha);
    } else {
        // 粗 dotted underline 用圆点近似, 每隔一个 thickness 放一个点.
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

        // 取左右两个候选圆点中距离更近的一个, 边缘做 1px 软化.
        let alpha = clamp(
            max(1.0 - (min(distance_left, distance_right) - radius), 0.0),
            0.0,
            1.0,
        ) * input.color.a;
        return vec4<f32>(input.color.rgb, alpha);
    }
}

@fragment
fn fs_dashed(input: VertexOutput) -> @location(0) vec4<f32> {
    let x = floor(((input.clip_position.x - uniforms.padding_x) % uniforms.cell_width));

    // 一个 cell 中间留空, 两侧绘制 dash. 相邻 cell 会自然拼接成连续虚线.
    let half_dash_len = floor(uniforms.cell_width / 4.0 + 0.5);

    var alpha: f32 = 1.0;
    if x > half_dash_len - 1.0 && x < uniforms.cell_width - half_dash_len {
        alpha = 0.0;
    }
    alpha = clamp(alpha, 0.0, 1.0) * input.color.a;

    return vec4<f32>(input.color.rgb, alpha);
}
