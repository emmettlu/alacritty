//! Win32 windows, messages and IMM input.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32,Ordering};
use std::time::Instant;
use windows::{core::{PCWSTR,w}, Win32::{Foundation::*, Graphics::Gdi::*, System::{LibraryLoader::GetModuleHandleW,Threading::GetCurrentThreadId}, UI::{HiDpi::*, Input::{KeyboardAndMouse::*, Ime::*}, Shell::*, WindowsAndMessaging::*}}};
use super::{dpi::{PhysicalPosition,PhysicalSize}, event::*, event_loop::{emit,ControlFlow}, keyboard::*, monitor::MonitorHandle, window::*};
static MAIN_THREAD:AtomicU32=AtomicU32::new(0);
thread_local! { static IME_ACTIVE:RefCell<HashMap<isize,bool>>=RefCell::new(HashMap::new()); }
fn wide(s:&str)->Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn io(err:windows::core::Error)->std::io::Error { std::io::Error::other(err.to_string()) }
pub fn initialize()->std::io::Result<()> {
    unsafe {
        let _=SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        MAIN_THREAD.store(GetCurrentThreadId(),Ordering::Release);
        let instance=GetModuleHandleW(None).map_err(io)?;
        let class=WNDCLASSEXW { cbSize:std::mem::size_of::<WNDCLASSEXW>() as u32,style:CS_HREDRAW|CS_VREDRAW,lpfnWndProc:Some(wndproc),hInstance:instance.into(),hCursor:LoadCursorW(None,IDC_IBEAM).map_err(io)?,lpszClassName:w!("AlacrittyNative"),..Default::default() };
        if RegisterClassExW(&class)==0 { return Err(std::io::Error::last_os_error()); }
    }
    Ok(())
}
pub fn wake() { let thread=MAIN_THREAD.load(Ordering::Acquire); if thread!=0 { unsafe { let _=PostThreadMessageW(thread,WM_APP,WPARAM(0),LPARAM(0)); } } }
pub fn drain_events() {
    unsafe { let mut message=MSG::default(); while PeekMessageW(&mut message,None,0,0,PM_REMOVE).as_bool() { if message.message==WM_QUIT { continue; } let _=DispatchMessageW(&message); } }
}
pub fn wait(flow:ControlFlow) { let timeout=match flow { ControlFlow::Poll=>return,ControlFlow::Wait=>u32::MAX,ControlFlow::WaitUntil(time)=>time.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128-1) as u32 }; unsafe { MsgWaitForMultipleObjectsEx(None,timeout,QS_ALLINPUT,MWMO_INPUTAVAILABLE); } }
fn modifiers()->Modifiers {
    unsafe {
        let down=|key:VIRTUAL_KEY|GetKeyState(key.0 as i32)<0;
        let mut state=ModifiersState::empty();
        if down(VK_SHIFT) { state|=ModifiersState::SHIFT; } if down(VK_CONTROL) { state|=ModifiersState::CONTROL; } if down(VK_MENU) { state|=ModifiersState::ALT; } if down(VK_LWIN)||down(VK_RWIN) { state|=ModifiersState::SUPER; }
        Modifiers { state,left_alt:down(VK_LMENU),right_alt:down(VK_RMENU) }
    }
}
fn key_event(vk:u32,raw:isize,pressed:bool)->KeyEvent {
    let scan=((raw>>16)&0xff) as u32; let extended=raw&(1<<24)!=0;
    let named=match vk { 0x08=>Some(NamedKey::Backspace),0x09=>Some(NamedKey::Tab),0x0d=>Some(NamedKey::Enter),0x1b=>Some(NamedKey::Escape),0x20=>Some(NamedKey::Space),0x21=>Some(NamedKey::PageUp),0x22=>Some(NamedKey::PageDown),0x23=>Some(NamedKey::End),0x24=>Some(NamedKey::Home),0x25=>Some(NamedKey::ArrowLeft),0x26=>Some(NamedKey::ArrowUp),0x27=>Some(NamedKey::ArrowRight),0x28=>Some(NamedKey::ArrowDown),0x2d=>Some(NamedKey::Insert),0x2e=>Some(NamedKey::Delete),0x10=>Some(NamedKey::Shift),0x11=>Some(NamedKey::Control),0x12=>Some(NamedKey::Alt),0x5b|0x5c=>Some(NamedKey::Super),0x70=>Some(NamedKey::F1),0x71=>Some(NamedKey::F2),0x72=>Some(NamedKey::F3),0x73=>Some(NamedKey::F4),0x74=>Some(NamedKey::F5),0x75=>Some(NamedKey::F6),0x76=>Some(NamedKey::F7),0x77=>Some(NamedKey::F8),0x78=>Some(NamedKey::F9),0x79=>Some(NamedKey::F10),0x7a=>Some(NamedKey::F11),0x7b=>Some(NamedKey::F12),_=>None };
    let mut state=[0u8;256]; let mut utf16=[0u16;16]; let mut plain=[0u16;16];
    let (text,base)=unsafe {
        let _=GetKeyboardState(&mut state); let layout=GetKeyboardLayout(0);
        let n=ToUnicodeEx(vk,scan,Some(&state),&mut utf16,4,layout);
        state[VK_CONTROL.0 as usize]=0; state[VK_MENU.0 as usize]=0; state[VK_SHIFT.0 as usize]=0;
        let p=ToUnicodeEx(vk,scan,Some(&state),&mut plain,4,layout);
        ((n>0).then(||String::from_utf16_lossy(&utf16[..n.max(0) as usize])),String::from_utf16_lossy(&plain[..p.max(0) as usize]))
    };
    let logical_key=named.map(Key::Named).unwrap_or_else(||Key::Character(text.clone().unwrap_or_else(||base.clone())));
    let unmodified_key=named.map(Key::Named).unwrap_or_else(||Key::Character(base.to_lowercase()));
    let location=if (0x60..=0x6f).contains(&vk)||(vk==0x0d&&extended) { KeyLocation::Numpad } else if matches!(vk,0x10|0x11|0x12) { if extended||scan==0x36 { KeyLocation::Right } else { KeyLocation::Left } } else { KeyLocation::Standard };
    KeyEvent { physical_key:PhysicalKey(scan|if extended { 0xe000 } else { 0 }),logical_key,text,unmodified_key,location,state:if pressed { ElementState::Pressed } else { ElementState::Released },repeat:pressed&&raw&(1<<30)!=0 }
}
unsafe fn ime_text(hwnd:HWND,kind:u32)->Option<String> {
    unsafe {
        let context=ImmGetContext(hwnd); if context.0.is_null() { return None; }
        let count=ImmGetCompositionStringW(context,IME_COMPOSITION_STRING(kind),None,0);
        let result=if count>0 { let mut data=vec![0u16;count as usize/2]; ImmGetCompositionStringW(context,IME_COMPOSITION_STRING(kind),Some(data.as_mut_ptr().cast()),count as u32); Some(String::from_utf16_lossy(&data)) } else { None };
        ImmReleaseContext(hwnd,context); result
    }
}
unsafe extern "system" fn wndproc(hwnd:HWND,message:u32,wparam:WPARAM,lparam:LPARAM)->LRESULT {
    let id=WindowId(hwnd.0 as u64);
    let send=|event|emit(id,event);
    unsafe {
        match message {
            WM_CLOSE=>{ send(WindowEvent::CloseRequested); return LRESULT(0); }
            WM_PAINT=>{ let mut paint=PAINTSTRUCT::default(); BeginPaint(hwnd,&mut paint); EndPaint(hwnd,&paint); send(WindowEvent::RedrawRequested); return LRESULT(0); }
            WM_SIZE=>send(WindowEvent::Resized(PhysicalSize::new((lparam.0&0xffff) as u32,((lparam.0>>16)&0xffff) as u32))),
            WM_DPICHANGED=>{ let dpi=(wparam.0&0xffff) as u32; send(WindowEvent::ScaleFactorChanged { scale_factor:dpi as f64/96. }); let rect=&*(lparam.0 as *const RECT); let _=SetWindowPos(hwnd,None,rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,SWP_NOZORDER|SWP_NOACTIVATE); }
            WM_SETFOCUS=>send(WindowEvent::Focused(true)), WM_KILLFOCUS=>{ send(WindowEvent::ModifiersChanged(Modifiers::default())); send(WindowEvent::Focused(false)); }
            WM_KEYDOWN|WM_SYSKEYDOWN|WM_KEYUP|WM_SYSKEYUP=>{
                send(WindowEvent::ModifiersChanged(modifiers()));
                if !IME_ACTIVE.with_borrow(|map|map.get(&(hwnd.0 as isize)).copied().unwrap_or(false)) {
                    let event=key_event(wparam.0 as u32,lparam.0,matches!(message,WM_KEYDOWN|WM_SYSKEYDOWN));
                    send(WindowEvent::KeyboardInput { device_id:DeviceId,event,is_synthetic:false });
                }
                if message==WM_SYSKEYDOWN&&wparam.0==VK_F4.0 as usize { return DefWindowProcW(hwnd,message,wparam,lparam); }
                return LRESULT(0);
            }
            WM_IME_STARTCOMPOSITION=>{ IME_ACTIVE.with_borrow_mut(|map|map.insert(hwnd.0 as isize,true)); send(WindowEvent::Ime(Ime::Enabled)); }
            WM_IME_ENDCOMPOSITION=>{ IME_ACTIVE.with_borrow_mut(|map|map.remove(&(hwnd.0 as isize))); send(WindowEvent::Ime(Ime::Preedit(String::new(),None))); }
            WM_IME_COMPOSITION=>{
                if lparam.0&GCS_RESULTSTR.0 as isize!=0 { if let Some(text)=ime_text(hwnd,GCS_RESULTSTR.0) { send(WindowEvent::Ime(Ime::Preedit(String::new(),None))); send(WindowEvent::Ime(Ime::Commit(text))); } }
                if lparam.0&GCS_COMPSTR.0 as isize!=0 { let text=ime_text(hwnd,GCS_COMPSTR.0).unwrap_or_default(); let context=ImmGetContext(hwnd); let cursor=ImmGetCompositionStringW(context,GCS_CURSORPOS,None,0).max(0) as usize; ImmReleaseContext(hwnd,context); let offset=text.char_indices().scan(0,|units,(byte,ch)|{ let before=*units; *units+=ch.len_utf16(); Some((before,byte)) }).find(|(units,_)|*units>=cursor).map_or(text.len(),|(_,byte)|byte); send(WindowEvent::Ime(Ime::Preedit(text,Some((offset,offset))))); }
                return LRESULT(0);
            }
            WM_MOUSEMOVE=>{
                let position=PhysicalPosition::new((lparam.0 as i16) as f64,((lparam.0>>16) as i16) as f64); send(WindowEvent::CursorMoved { device_id:DeviceId,position });
                let mut track=TRACKMOUSEEVENT { cbSize:std::mem::size_of::<TRACKMOUSEEVENT>() as u32,dwFlags:TME_LEAVE,hwndTrack:hwnd,..Default::default() }; let _=TrackMouseEvent(&mut track);
            }
            WM_MOUSELEAVE=>send(WindowEvent::CursorLeft { device_id:DeviceId }),
            WM_LBUTTONDOWN|WM_LBUTTONUP|WM_RBUTTONDOWN|WM_RBUTTONUP|WM_MBUTTONDOWN|WM_MBUTTONUP|WM_XBUTTONDOWN|WM_XBUTTONUP=>{
                let pressed=matches!(message,WM_LBUTTONDOWN|WM_RBUTTONDOWN|WM_MBUTTONDOWN|WM_XBUTTONDOWN);
                if pressed { SetCapture(hwnd); } else { let _=ReleaseCapture(); }
                let button=match message { WM_LBUTTONDOWN|WM_LBUTTONUP=>MouseButton::Left,WM_RBUTTONDOWN|WM_RBUTTONUP=>MouseButton::Right,WM_MBUTTONDOWN|WM_MBUTTONUP=>MouseButton::Middle,_=>if wparam.0>>16==1 { MouseButton::Back } else { MouseButton::Forward } };
                send(WindowEvent::ModifiersChanged(modifiers())); send(WindowEvent::MouseInput { device_id:DeviceId,state:if pressed { ElementState::Pressed } else { ElementState::Released },button });
            }
            WM_MOUSEWHEEL|WM_MOUSEHWHEEL=>{ let amount=((wparam.0>>16) as i16) as f32/120.; let delta=if message==WM_MOUSEHWHEEL { MouseScrollDelta::LineDelta(-amount,0.) } else { MouseScrollDelta::LineDelta(0.,amount) }; send(WindowEvent::ModifiersChanged(modifiers())); send(WindowEvent::MouseWheel { device_id:DeviceId,delta,phase:TouchPhase::Moved }); }
            WM_DROPFILES=>{ let drop=HDROP(wparam.0 as _); let count=DragQueryFileW(drop,u32::MAX,None); for index in 0..count { let len=DragQueryFileW(drop,index,None); let mut path=vec![0u16;len as usize+1]; DragQueryFileW(drop,index,Some(&mut path)); send(WindowEvent::DroppedFile(String::from_utf16_lossy(&path[..len as usize]).into())); } DragFinish(drop); }
            WM_DESTROY=>{ IME_ACTIVE.with_borrow_mut(|map|map.remove(&(hwnd.0 as isize))); }
            _=>{}
        }
        DefWindowProcW(hwnd,message,wparam,lparam)
    }
}
pub struct Window { hwnd:HWND, fullscreen:Cell<bool>, maximized:Cell<bool>, cursor:Cell<HCURSOR>, cursor_visible:Cell<bool>, saved:Cell<RECT> }
impl Window {
    pub fn new(attrs:WindowAttributes)->std::io::Result<Self> {
        unsafe {
            let instance=GetModuleHandleW(None).map_err(io)?; let title=wide(&attrs.title);
            let style=if attrs.decorations==crate::config::window::Decorations::None { WS_POPUP|WS_THICKFRAME } else { WS_OVERLAPPEDWINDOW };
            let extended=if attrs.transparent { WS_EX_NOREDIRECTIONBITMAP } else { WINDOW_EX_STYLE::default() };
            let hwnd=CreateWindowExW(extended,w!("AlacrittyNative"),PCWSTR(title.as_ptr()),style,attrs.position.map_or(CW_USEDEFAULT,|p|p.x),attrs.position.map_or(CW_USEDEFAULT,|p|p.y),attrs.size.width as i32,attrs.size.height as i32,None,None,Some(instance.into()),None).map_err(io)?;
            DragAcceptFiles(hwnd,true);
            if attrs.level==WindowLevel::AlwaysOnTop { SetWindowPos(hwnd,Some(HWND_TOPMOST),0,0,0,0,SWP_NOMOVE|SWP_NOSIZE).map_err(io)?; }
            let window=Self { hwnd,fullscreen:Cell::new(false),maximized:Cell::new(false),cursor:Cell::new(LoadCursorW(None,IDC_IBEAM).map_err(io)?),cursor_visible:Cell::new(true),saved:Cell::new(RECT::default()) };
            if attrs.maximized { window.set_maximized(true); } if attrs.fullscreen { window.set_fullscreen(Some(Fullscreen::Borderless(None))); }
            Ok(window)
        }
    }
    pub fn hwnd(&self)->HWND { self.hwnd }
    pub fn id(&self)->WindowId { WindowId(self.hwnd.0 as u64) }
    pub fn inner_size(&self)->PhysicalSize<u32> { unsafe { let mut rect=RECT::default(); let _=GetClientRect(self.hwnd,&mut rect); PhysicalSize::new((rect.right-rect.left).max(0) as u32,(rect.bottom-rect.top).max(0) as u32) } }
    pub fn scale_factor(&self)->f64 { unsafe { GetDpiForWindow(self.hwnd) as f64/96. } }
    pub fn request_inner_size(&self,size:PhysicalSize<u32>)->Option<PhysicalSize<u32>> { unsafe { let style=WINDOW_STYLE(GetWindowLongW(self.hwnd,GWL_STYLE) as u32); let ex=WINDOW_EX_STYLE(GetWindowLongW(self.hwnd,GWL_EXSTYLE) as u32); let mut rect=RECT { left:0,top:0,right:size.width as i32,bottom:size.height as i32 }; let _=AdjustWindowRectExForDpi(&mut rect,style,false,ex,GetDpiForWindow(self.hwnd)); let _=SetWindowPos(self.hwnd,None,0,0,rect.right-rect.left,rect.bottom-rect.top,SWP_NOMOVE|SWP_NOZORDER|SWP_NOACTIVATE); } Some(self.inner_size()) }
    pub fn set_visible(&self,visible:bool) { unsafe { let _=ShowWindow(self.hwnd,if visible { SW_SHOW } else { SW_HIDE }); } }
    pub fn set_title(&self,title:&str) { unsafe { let _=SetWindowTextW(self.hwnd,PCWSTR(wide(title).as_ptr())); } }
    pub fn request_redraw(&self) { emit(self.id(),WindowEvent::RedrawRequested); }
    pub fn set_cursor(&self,cursor:CursorIcon) { unsafe { if let Ok(cursor)=LoadCursorW(None,match cursor { CursorIcon::Default=>IDC_ARROW,CursorIcon::Text=>IDC_IBEAM,CursorIcon::Pointer=>IDC_HAND }) { self.cursor.set(cursor); SetCursor(Some(cursor)); SetClassLongPtrW(self.hwnd,GCLP_HCURSOR,cursor.0 as isize); } } }
    pub fn set_cursor_visible(&self,visible:bool) { if self.cursor_visible.replace(visible)!=visible { unsafe { ShowCursor(visible); } } }
    pub fn set_ime_allowed(&self,allowed:bool) { unsafe { ImmAssociateContextEx(self.hwnd,None,if allowed { IACE_DEFAULT } else { IACE_IGNORENOCONTEXT }); } }
    pub fn set_ime_purpose(&self,_purpose:ImePurpose) {}
    pub fn set_ime_cursor_area(&self,position:PhysicalPosition<f64>,size:PhysicalSize<f64>) { unsafe { let context=ImmGetContext(self.hwnd); let area=COMPOSITIONFORM { dwStyle:CFS_POINT,ptCurrentPos:POINT { x:position.x as i32,y:(position.y+size.height) as i32 },..Default::default() }; let _=ImmSetCompositionWindow(context,&area); let candidate=CANDIDATEFORM { dwIndex:0,dwStyle:CFS_EXCLUDE,ptCurrentPos:POINT { x:position.x as i32,y:position.y as i32 },rcArea:RECT { left:position.x as i32,top:position.y as i32,right:(position.x+size.width) as i32,bottom:(position.y+size.height) as i32 } }; let _=ImmSetCandidateWindow(context,&candidate); ImmReleaseContext(self.hwnd,context); } }
    pub fn set_transparent(&self,_transparent:bool) {}
    pub fn set_blur(&self,_blur:bool) {}
    pub fn set_maximized(&self,maximized:bool) { self.maximized.set(maximized); unsafe { let _=ShowWindow(self.hwnd,if maximized { SW_MAXIMIZE } else { SW_RESTORE }); } }
    pub fn is_maximized(&self)->bool { unsafe { IsZoomed(self.hwnd).as_bool() } }
    pub fn set_minimized(&self,minimized:bool) { unsafe { let _=ShowWindow(self.hwnd,if minimized { SW_MINIMIZE } else { SW_RESTORE }); } }
    pub fn set_resize_increments(&self,_increments:Option<PhysicalSize<f32>>) {}
    pub fn fullscreen(&self)->Option<Fullscreen> { self.fullscreen.get().then_some(Fullscreen::Borderless(None)) }
    pub fn set_fullscreen(&self,mode:Option<Fullscreen>) { if self.fullscreen.replace(mode.is_some())==mode.is_some() { return; } unsafe { if mode.is_some() { let mut rect=RECT::default(); let _=GetWindowRect(self.hwnd,&mut rect); self.saved.set(rect); SetWindowLongW(self.hwnd,GWL_STYLE,(WS_POPUP|WS_VISIBLE).0 as i32); let monitor=MonitorFromWindow(self.hwnd,MONITOR_DEFAULTTONEAREST); let mut info=MONITORINFO { cbSize:std::mem::size_of::<MONITORINFO>() as u32,..Default::default() }; if GetMonitorInfoW(monitor,&mut info).as_bool() { let r=info.rcMonitor; let _=SetWindowPos(self.hwnd,None,r.left,r.top,r.right-r.left,r.bottom-r.top,SWP_FRAMECHANGED|SWP_NOZORDER); } } else { let rect=self.saved.get(); SetWindowLongW(self.hwnd,GWL_STYLE,WS_OVERLAPPEDWINDOW.0 as i32); let _=SetWindowPos(self.hwnd,None,rect.left,rect.top,rect.right-rect.left,rect.bottom-rect.top,SWP_FRAMECHANGED|SWP_NOZORDER); } } }
    pub fn current_monitor(&self)->Option<MonitorHandle> { unsafe { let monitor=MonitorFromWindow(self.hwnd,MONITOR_DEFAULTTONEAREST); let mut info=MONITORINFOEXW::default(); info.monitorInfo.cbSize=std::mem::size_of::<MONITORINFOEXW>() as u32; if !GetMonitorInfoW(monitor,&mut info as *mut _ as _).as_bool() { return None; } let mut mode=DEVMODEW { dmSize:std::mem::size_of::<DEVMODEW>() as u16,..Default::default() }; EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()),ENUM_CURRENT_SETTINGS,&mut mode).as_bool().then_some(MonitorHandle { refresh_rate:mode.dmDisplayFrequency.max(1)*1000 }) } }
    pub fn pre_present_notify(&self) {}
    pub fn request_user_attention(&self,attention:Option<UserAttentionType>) { unsafe { let info=FLASHWINFO { cbSize:std::mem::size_of::<FLASHWINFO>() as u32,hwnd:self.hwnd,dwFlags:if attention.is_some() { FLASHW_TRAY|FLASHW_TIMERNOFG } else { FLASHW_STOP },uCount:u32::MAX,dwTimeout:0 }; FlashWindowEx(&info); } }
}
impl Drop for Window { fn drop(&mut self) { self.set_cursor_visible(true); unsafe { let _=DestroyWindow(self.hwnd); } } }
