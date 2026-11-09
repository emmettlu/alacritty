#include <metal_stdlib>
using namespace metal;
struct Uniforms { packed_float4 projection; packed_float2 cell; packed_float2 padding; float underline; float thickness; float curl; float pad; };
struct Text { packed_ushort2 grid; packed_short4 glyph; packed_float4 uv; packed_uchar4 fg; packed_uchar4 bg; };
struct Rect { packed_float4 rect; packed_float4 color; uint kind; };
struct Out { float4 position [[position]]; float2 uv; float4 color [[flat]]; uint kind [[flat]]; };
float2 corner(uint vid) { constexpr float2 corners[6]={float2(1,0),float2(1,1),float2(0,0),float2(1,1),float2(0,1),float2(0,0)}; return corners[vid]; }
vertex Out vs_bg(uint vid [[vertex_id]],uint iid [[instance_id]],constant Text* data [[buffer(0)]],constant Uniforms& u [[buffer(1)]]) {
    Text t=data[iid]; float2 size=float2(u.cell); if(t.fg.w&2)size.x*=2;
    float2 pos=float2(t.grid)*float2(u.cell)+corner(vid)*size;
    return {float4(float2(u.projection.xy)+float2(u.projection.zw)*pos,0,1),float2(0),float4(t.bg)/255.,0};
}
vertex Out vs_text(uint vid [[vertex_id]],uint iid [[instance_id]],constant Text* data [[buffer(0)]],constant Uniforms& u [[buffer(1)]]) {
    Text t=data[iid]; float2 c=corner(vid); float2 pos=float2(t.grid)*float2(u.cell)+float2(t.glyph.x,u.cell.y-t.glyph.y)+float2(t.glyph.zw)*c;
    return {float4(float2(u.projection.xy)+float2(u.projection.zw)*pos,0,1),float2(t.uv.xy)+float2(t.uv.zw)*c,float4(float3(t.fg.xyz)/255.,1),uint(t.fg.w)};
}
fragment float4 fs_bg(Out in [[stage_in]]) { if(in.color.a==0)discard_fragment(); return float4(in.color.rgb*in.color.a,in.color.a); }
fragment float4 fs_text(Out in [[stage_in]],texture2d<float> atlas [[texture(0)]]) {
    constexpr sampler sample(coord::normalized,address::clamp_to_edge,filter::nearest);
    float4 mask=atlas.sample(sample,in.uv); if(in.kind&1)return mask; if(mask.a<.001)discard_fragment(); return float4(in.color.rgb,mask.a);
}
vertex Out vs_rect(uint vid [[vertex_id]],uint iid [[instance_id]],constant Rect* data [[buffer(0)]],constant Uniforms& u [[buffer(1)]]) {
    Rect r=data[iid]; float2 pos=float2(r.rect.xy)+corner(vid)*float2(r.rect.zw);
    return {float4(float2(u.projection.xy)+float2(u.projection.zw)*pos,0,1),float2(0),float4(r.color),r.kind};
}
fragment float4 fs_rect(Out in [[stage_in]],constant Uniforms& u [[buffer(1)]]) {
    float x=floor(fmod(in.position.x-u.padding.x,u.cell.x)); float y=floor(fmod(in.position.y-u.padding.y,u.cell.y)); float alpha=1;
    if(in.kind==1) {
        float center=u.curl*.5*cos((x+.5)*6.2831853076/u.cell.x)+u.curl-1;
        float half_extra=max(u.thickness-1.,0.)*.5; float distance=max(y-center-half_extra,max(center-half_extra-y,0.)); alpha=clamp(1-distance*distance,0.,1.);
    } else if(in.kind==2) {
        if(u.thickness<2) { float even=int(u.cell.x)%2!=0?fmod((in.position.x-u.padding.x)/u.cell.x,2.):0; alpha=int(x)%2!=int(even)?0:clamp(1-abs(floor(u.underline)-y),0.,1.); }
        else { float n=floor(x/u.thickness); float radius=u.thickness*.5; float left=(n-fmod(n,2.))*u.thickness+radius; float dy=y-u.underline+1; float a=length(float2(x-left,dy)); float b=length(float2(x-left-2*u.thickness,dy)); alpha=clamp(1-min(a,b)+radius,0.,1.); }
    } else if(in.kind==3) { float half_dash=floor(u.cell.x*.25+.5); if(x>half_dash-1&&x<u.cell.x-half_dash)alpha=0; }
    return float4(in.color.rgb,in.color.a*alpha);
}
