cbuffer Uniforms : register(b0) { float4 projection; float2 cell; float2 padding; float underline; float thickness; float curl; float pad; };
Texture2D<float4> atlas : register(t0); SamplerState atlas_sampler : register(s0);
struct Text { uint2 grid:GRID; int4 glyph:GLYPH; float4 uv:UV; uint4 fg:FG; float4 bg:BG; };
struct Rect { float4 rect:RECT; float4 color:COLOR; uint kind:KIND; };
struct Out { float4 pos:SV_Position; float2 uv:UV; nointerpolation float4 color:COLOR; nointerpolation uint kind:KIND; };
float2 corner(uint vid) { const float2 corners[6]={float2(1,0),float2(1,1),float2(0,0),float2(1,1),float2(0,1),float2(0,0)}; return corners[vid]; }
Out vs_bg(Text t,uint vid:SV_VertexID) { Out o; float2 size=cell; if(t.fg.w&2)size.x*=2; float2 pos=float2(t.grid)*cell+corner(vid)*size; o.pos=float4(projection.xy+projection.zw*pos,0,1); o.uv=0; o.color=t.bg; o.kind=0; return o; }
Out vs_text(Text t,uint vid:SV_VertexID) { Out o; float2 c=corner(vid); float2 pos=float2(t.grid)*cell+float2(t.glyph.x,cell.y-t.glyph.y)+float2(t.glyph.zw)*c; o.pos=float4(projection.xy+projection.zw*pos,0,1); o.uv=t.uv.xy+t.uv.zw*c; o.color=float4(float3(t.fg.xyz)/255.,1); o.kind=t.fg.w; return o; }
float4 fs_bg(Out i):SV_Target { if(i.color.a==0)discard; return float4(i.color.rgb*i.color.a,i.color.a); }
float4 fs_text(Out i):SV_Target { float4 mask=atlas.Sample(atlas_sampler,i.uv); if(i.kind&1)return mask; if(mask.a<.001)discard; return float4(i.color.rgb,mask.a); }
Out vs_rect(Rect r,uint vid:SV_VertexID) { Out o; float2 pos=r.rect.xy+corner(vid)*r.rect.zw; o.pos=float4(projection.xy+projection.zw*pos,0,1); o.uv=0; o.color=r.color; o.kind=r.kind; return o; }
float4 fs_rect(Out i):SV_Target {
    float x=floor(fmod(i.pos.x-padding.x,cell.x)); float y=floor(fmod(i.pos.y-padding.y,cell.y)); float alpha=1;
    if(i.kind==1) { float center=curl*.5*cos((x+.5)*6.2831853076/cell.x)+curl-1; float extra=max(thickness-1.,0.)*.5; float distance=max(y-center-extra,max(center-extra-y,0.)); alpha=saturate(1-distance*distance); }
    else if(i.kind==2) { if(thickness<2) { float even=int(cell.x)%2!=0?fmod((i.pos.x-padding.x)/cell.x,2.):0; alpha=int(x)%2!=int(even)?0:saturate(1-abs(floor(underline)-y)); } else { float n=floor(x/thickness); float radius=thickness*.5; float left=(n-fmod(n,2.))*thickness+radius; float dy=y-underline+1; float a=length(float2(x-left,dy)); float b=length(float2(x-left-2*thickness,dy)); alpha=saturate(1-min(a,b)+radius); } }
    else if(i.kind==3) { float half_dash=floor(cell.x*.25+.5); if(x>half_dash-1&&x<cell.x-half_dash)alpha=0; }
    return float4(i.color.rgb,i.color.a*alpha);
}
