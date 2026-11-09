//! CoreText rasterization through cidre.
use std::{collections::HashMap,fmt};
use cidre::{arc, cf, cg, ct, ns, objc};

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct FontDesc { name:String,style:Style }
impl FontDesc { pub fn new(name:impl Into<String>,style:Style)->Self { Self { name:name.into(),style } } }
impl fmt::Display for FontDesc { fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { write!(f,"{} ({:?})",self.name,self.style) } }
#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub enum Slant { Normal,Italic }
#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub enum Weight { Normal,Bold }
#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Style { Specific(String),Description { slant:Slant,weight:Weight } }
#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub struct FontKey(usize);
#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
pub struct GlyphKey { pub character:char,pub font_key:FontKey,pub size:Size }
#[derive(Debug,Clone,Copy,PartialEq,Eq,PartialOrd,Ord,Hash)]
pub struct Size(u32);
impl Size {
    pub fn new(points:f32)->Self { Self((points.clamp(1.,3999.)*1_000_000.) as u32) }
    pub fn from_px(pixels:f32)->Self { Self::new(pixels*72./96.) }
    pub fn as_pt(self)->f32 { (f64::from(self.0)/1_000_000.) as f32 }
    pub fn as_px(self)->f32 { self.as_pt()*96./72. }
    pub fn scale(self,factor:f32)->Self { Self::new(self.as_pt()*factor) }
}
#[derive(Debug,Clone)]
pub enum BitmapBuffer { Rgb(Vec<u8>),Rgba(Vec<u8>) }
#[derive(Debug,Clone)]
pub struct RasterizedGlyph { pub character:char,pub width:i32,pub height:i32,pub top:i32,pub left:i32,pub advance:(i32,i32),pub buffer:BitmapBuffer }
impl Default for RasterizedGlyph { fn default()->Self { Self { character:' ',width:0,height:0,top:0,left:0,advance:(0,0),buffer:BitmapBuffer::Rgb(Vec::new()) } } }
#[derive(Debug,Clone,Copy)]
pub struct Metrics { pub average_advance:f64,pub line_height:f64,pub descent:f32,pub underline_position:f32,pub underline_thickness:f32,pub strikeout_position:f32,pub strikeout_thickness:f32 }
#[derive(Debug)]
pub enum Error { FontNotFound(FontDesc),MissingGlyph(RasterizedGlyph),UnknownFontKey,PlatformError(String) }
impl std::error::Error for Error {}
impl fmt::Display for Error { fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { match self { Self::FontNotFound(desc)=>write!(f,"Font not found: {desc}"),Self::MissingGlyph(glyph)=>write!(f,"Missing glyph: {}",glyph.character),Self::UnknownFontKey=>f.write_str("Unknown font key"),Self::PlatformError(error)=>f.write_str(error) } } }
pub trait Rasterize:Sized {
    fn new()->Result<Self,Error>;
    fn load_font(&mut self,desc:&FontDesc,size:Size)->Result<FontKey,Error>;
    fn metrics(&self,key:FontKey,size:Size)->Result<Metrics,Error>;
    fn get_glyph(&mut self,key:GlyphKey)->Result<RasterizedGlyph,Error>;
}
pub struct Rasterizer { fonts:Vec<arc::R<ct::Font>>,keys:HashMap<(FontDesc,Size),FontKey> }
trait FontManagerApi:objc::Obj {
    #[objc::msg_send(fontWithFamily:traits:weight:size:)]
    fn family_font(&self,family:&ns::String,traits:usize,weight:isize,size:f64)->Option<arc::R<ns::Font>>;
    #[objc::msg_send(availableMembersOfFontFamily:)]
    fn family_members(&self,family:&ns::String)->Option<arc::R<ns::Array<ns::Array<ns::Id>>>>;
}
impl FontManagerApi for ns::FontManager {}
#[link(name="CoreText",kind="framework")]
unsafe extern "C" {
    fn CTFontCreateForString(font:&ct::Font,string:&cf::String,range:cf::Range)->arc::R<ct::Font>;
    fn CTFontDrawGlyphs(font:&ct::Font,glyphs:*const cg::Glyph,positions:*const cg::Point,count:cf::Index,context:&mut cg::Context);
}
fn pool<T>(f:impl FnOnce()->T)->T { let mut value=None; objc::ar_pool(||value=Some(f())); value.unwrap() }
impl Rasterize for Rasterizer {
    fn new()->Result<Self,Error> { Ok(Self { fonts:Vec::new(),keys:HashMap::new() }) }
    fn load_font(&mut self,desc:&FontDesc,size:Size)->Result<FontKey,Error> {
        if let Some(key)=self.keys.get(&(desc.clone(),size)) { return Ok(*key); }
        let font=pool(|| {
            let manager=ns::FontManager::shared(); let family=ns::String::with_str(&desc.name);
            match &desc.style {
                Style::Description { slant,weight }=>manager.family_font(&family,usize::from(*slant==Slant::Italic)|if *weight==Weight::Bold { 2 } else { 0 },if *weight==Weight::Bold { 9 } else { 5 },size.as_pt() as f64),
                Style::Specific(style)=>{
                    let members=manager.family_members(&family)?;
                    for member in members.iter() {
                        if member.len()<2 { continue; }
                        let name=member.get(0).ok()?; let member_style=member.get(1).ok()?;
                        use cidre::objc::Obj;
                        if member_style.try_cast(ns::String::cls()).is_some_and(|s|s.to_string()==*style) {
                            return ns::Font::with_name_size(name.try_cast(ns::String::cls())?,size.as_pt() as f64);
                        }
                    }
                    None
                }
            }
        }).ok_or_else(||Error::FontNotFound(desc.clone()))?;
        let key=FontKey(self.fonts.len()); self.fonts.push(font.as_ct().retained()); self.keys.insert((desc.clone(),size),key); Ok(key)
    }
    fn metrics(&self,key:FontKey,_size:Size)->Result<Metrics,Error> {
        let font=self.fonts.get(key.0).ok_or(Error::UnknownFontKey)?; let mut glyph=[cg::Glyph(cg::FontIndex(0))]; let _=font.glyphs_for_characters(&['0' as u16],&mut glyph);
        let descent=font.descent().round(); let height=font.ascent().round()+descent+font.leading().round(); let thickness=font.underline_thickness() as f32;
        Ok(Metrics { average_advance:font.advance_for_glyphs(ct::FontOrientation::Default,&glyph),line_height:height,descent:-descent as f32,underline_position:font.underline_pos() as f32,underline_thickness:thickness,strikeout_position:(height*0.5-descent) as f32,strikeout_thickness:thickness })
    }
    fn get_glyph(&mut self,key:GlyphKey)->Result<RasterizedGlyph,Error> {
        let base=self.fonts.get(key.font_key.0).ok_or(Error::UnknownFontKey)?;
        let mut utf16=[0;2]; let text=key.character.encode_utf16(&mut utf16); let mut glyphs=[cg::Glyph(cg::FontIndex(0));2];
        let fallback;
        let font=if base.glyphs_for_characters(text,&mut glyphs).is_ok() { base.as_ref() } else {
            let string=cf::String::from_str(&key.character.to_string()); fallback=unsafe { CTFontCreateForString(base,&string,cf::Range::new(0,text.len() as cf::Index)) };
            let _=fallback.glyphs_for_characters(text,&mut glyphs); fallback.as_ref()
        };
        let glyph=glyphs[0]; let bounds=font.bounding_rect_for_glyphs(ct::FontOrientation::Default,&[glyph]);
        let left=bounds.origin.x.floor() as i32; let top=(bounds.origin.y+bounds.size.height).ceil() as i32; let bottom=bounds.origin.y.floor() as i32;
        let width=(bounds.origin.x+bounds.size.width).ceil() as i32-left; let height=top-bottom;
        if width<=0||height<=0 { return Ok(RasterizedGlyph { character:key.character,..RasterizedGlyph::default() }); }
        let width=width as usize; let height=height as usize; let mut pixels=vec![0u8;width*height*4];
        let space=cg::ColorSpace::device_rgb().ok_or_else(||Error::PlatformError("Cannot create font color space".into()))?;
        let mut context=cg::Context::new_bitmap(pixels.as_mut_ptr().cast(),width,height,8,width*4,&space,cg::BitmapInfo::with_alpha(cg::ImageAlphaInfo::PremultipliedLast)|cg::BitmapInfo::BYTE_ORDER_32_BIG).ok_or_else(||Error::PlatformError("Cannot create glyph bitmap context".into()))?;
        context.set_rgb_fill_color(1.,1.,1.,1.); context.set_text_pos(0.,0.);
        unsafe { CTFontDrawGlyphs(font,&glyph,&cg::Point::new(-left as f64,-bottom as f64),1,&mut context); }
        drop(context);
        let colored=font.symbolic_traits().0&(1<<13)!=0;
        let buffer=if colored {
            // The render pipeline expects straight alpha; CoreGraphics produces premultiplied sRGB.
            for pixel in pixels.chunks_exact_mut(4) { if pixel[3]!=0 { let alpha=pixel[3] as u32; for channel in &mut pixel[..3] { *channel=((*channel as u32*255+alpha/2)/alpha).min(255) as u8; } } }
            BitmapBuffer::Rgba(pixels)
        } else { let mut mask=Vec::with_capacity(width*height*3); for pixel in pixels.chunks_exact(4) { mask.extend_from_slice(&[pixel[3];3]); } BitmapBuffer::Rgb(mask) };
        let rasterized=RasterizedGlyph { character:key.character,width:width as i32,height:height as i32,left,top,advance:(font.advance_for_glyphs(ct::FontOrientation::Default,&[glyph]).round() as i32,0),buffer };
        if glyph.0.0==0 { Err(Error::MissingGlyph(rasterized)) } else { Ok(rasterized) }
    }
}
