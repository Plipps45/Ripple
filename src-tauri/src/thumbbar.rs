use std::cell::RefCell;
use std::sync::OnceLock;

use serde_json::json;
use tauri::{AppHandle, WebviewWindow};
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, SetWindowSubclass, TaskbarList, THBF_ENABLED, THBN_CLICKED, THB_FLAGS, THB_ICON, THB_TOOLTIP,
    THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIcon, DestroyIcon, GetSystemMetrics, RegisterWindowMessageW, HICON, SM_CXSMICON, WM_COMMAND};

const BUTTONS: [&str; 5] = ["shuffle", "prev", "toggle", "next", "repeat"];

macro_rules! glyph {
    ($name:literal) => {
        [
            (16u32, include_bytes!(concat!("../icons/thumbbar/", $name, "-16.png")) as &[u8]),
            (20, include_bytes!(concat!("../icons/thumbbar/", $name, "-20.png")) as &[u8]),
            (24, include_bytes!(concat!("../icons/thumbbar/", $name, "-24.png")) as &[u8]),
            (32, include_bytes!(concat!("../icons/thumbbar/", $name, "-32.png")) as &[u8]),
        ]
    };
}
type Glyph = [(u32, &'static [u8]); 4];
const SHUFFLE: Glyph = glyph!("shuffle");
const REPEAT: Glyph = glyph!("repeat");
const PREV: Glyph = glyph!("prev");
const NEXT: Glyph = glyph!("next");
const PLAY: Glyph = glyph!("play");
const PAUSE: Glyph = glyph!("pause");

#[derive(Clone, PartialEq)]
pub struct View {
    pub playing: bool,
    pub shuffle: bool,
    pub repeat: bool,
    pub accent: u32,
}

struct Thumbbar {
    list: ITaskbarList3,
    hwnd: HWND,
    added: bool,
    view: View,
    icons: Vec<HICON>,
}

static APP: OnceLock<AppHandle> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

thread_local! {
    static BAR: RefCell<Option<Thumbbar>> = const { RefCell::new(None) };
}

fn light_taskbar() -> bool {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && value == 1
}

fn make_icon(glyph: &Glyph, rgb: u32, opacity: f32) -> Option<HICON> {
    let want = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    let (_, png) = glyph.iter().find(|(s, _)| *s >= want).unwrap_or(&glyph[glyph.len() - 1]);
    let img = tauri::image::Image::from_bytes(png).ok()?;
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let bgra: Vec<u8> = img
        .rgba()
        .chunks_exact(4)
        .flat_map(|p| [b, g, r, (p[3] as f32 * opacity).round() as u8])
        .collect();
    let mask = vec![0u8; (img.width().div_ceil(16) * 2 * img.height()) as usize];
    unsafe { CreateIcon(None, img.width() as i32, img.height() as i32, 1, 32, mask.as_ptr(), bgra.as_ptr()).ok() }
}

fn tip(text: &str) -> [u16; 260] {
    let mut out = [0u16; 260];
    for (i, c) in text.encode_utf16().take(259).enumerate() {
        out[i] = c;
    }
    out
}

impl Thumbbar {
    fn buttons(&mut self) -> Vec<THUMBBUTTON> {
        for icon in self.icons.drain(..) {
            let _ = unsafe { DestroyIcon(icon) };
        }
        let fg = if light_taskbar() { 0x1b1b1f } else { 0xffffff };
        let toggle = |on: bool| if on { (self.view.accent, 1.0) } else { (fg, 0.45) };
        let v = &self.view;
        let spec: [(&Glyph, (u32, f32), &str); 5] = [
            (&SHUFFLE, toggle(v.shuffle), if v.shuffle { "Shuffle: on" } else { "Shuffle: off" }),
            (&PREV, (fg, 1.0), "Previous"),
            (if v.playing { &PAUSE } else { &PLAY }, (fg, 1.0), if v.playing { "Pause" } else { "Play" }),
            (&NEXT, (fg, 1.0), "Next"),
            (&REPEAT, toggle(v.repeat), if v.repeat { "Repeat: on" } else { "Repeat: off" }),
        ];
        let mut out = Vec::with_capacity(spec.len());
        for (id, (glyph, (rgb, opacity), text)) in spec.into_iter().enumerate() {
            let icon = make_icon(glyph, rgb, opacity).unwrap_or_default();
            self.icons.push(icon);
            out.push(THUMBBUTTON {
                dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
                iId: id as u32,
                iBitmap: 0,
                hIcon: icon,
                szTip: tip(text),
                dwFlags: THBF_ENABLED,
            });
        }
        out
    }

    fn add(&mut self) {
        let buttons = self.buttons();
        let result = unsafe { self.list.ThumbBarAddButtons(self.hwnd, &buttons) };
        match &result {
            Ok(()) => crate::logger::log("info", "[thumbbar] buttons added"),
            Err(e) => crate::logger::log("warn", &format!("[thumbbar] could not add the buttons: {e}")),
        }
        self.added = result.is_ok();
    }

    fn refresh(&mut self) {
        if !self.added {
            return;
        }
        let buttons = self.buttons();
        let _ = unsafe { self.list.ThumbBarUpdateButtons(self.hwnd, &buttons) };
    }
}

unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if Some(&msg) == TASKBAR_CREATED.get() {
        BAR.with(|bar| {
            if let Some(bar) = bar.borrow_mut().as_mut() {
                bar.add();
            }
        });
    } else if msg == WM_COMMAND && (wparam.0 >> 16) as u32 & 0xffff == THBN_CLICKED {
        if let (Some(cmd), Some(app)) = (BUTTONS.get(wparam.0 & 0xffff), APP.get()) {
            crate::commands::emit_main(app, "media-command", json!(cmd));
        }
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

pub fn install(app: &AppHandle, window: &WebviewWindow) {
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0 as _);
    let _ = APP.set(app.clone());
    let _ = TASKBAR_CREATED.set(unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) });
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let list: ITaskbarList3 = match CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) {
            Ok(list) => list,
            Err(e) => {
                crate::logger::log("warn", &format!("[thumbbar] no taskbar list: {e}"));
                return;
            }
        };
        if list.HrInit().is_err() {
            return;
        }
        BAR.with(|bar| {
            *bar.borrow_mut() = Some(Thumbbar {
                list,
                hwnd,
                added: false,
                view: View { playing: false, shuffle: false, repeat: false, accent: 0xffffff },
                icons: Vec::new(),
            })
        });
        let _: BOOL = SetWindowSubclass(hwnd, Some(subclass_proc), 0x5249_5050, 0);
    }
}

pub fn set_state(app: &AppHandle, view: View) {
    let _ = app.run_on_main_thread(move || {
        BAR.with(|bar| {
            if let Some(bar) = bar.borrow_mut().as_mut() {
                if bar.view != view {
                    bar.view = view;
                    bar.refresh();
                }
            }
        });
    });
}
