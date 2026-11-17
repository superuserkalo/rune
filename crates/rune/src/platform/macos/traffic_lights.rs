//! The traffic lights at the app's own spot (`Chrome::traffic_lights`).
//!
//! Since macOS 26, AppKit puts the buttons back at (9, 9) and the titlebar container back to
//! its own height whenever it lays the titlebar out again, which it does on its own (a title
//! or appearance change, a zoom), not only on the resizes the shell sees. So the classes of the
//! container and of the buttons' own superview get a `-layout` hook that runs AppKit's and then
//! puts the buttons back. The hook sits on the class, not on a per-instance subclass: on macOS 26
//! AppKit already observes the container with KVO before Rune sees it, and moving such a view to
//! another class hides KVO's own subclass, so AppKit's observer gets no value and aborts.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CStr;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::sel;
use objc2_app_kit::{NSView, NSWindow, NSWindowButton, NSWindowStyleMask};
use objc2_foundation::NSPoint;
use winit::window::Window;

use crate::platform::macos::ns_window;

thread_local! {
    /// Each window's spot, by its `NSWindow` pointer, for the layout hook.
    static SPOTS: RefCell<HashMap<usize, (f64, f64)>> = RefCell::new(HashMap::new());
    /// Set while the hook places, so the frames it sets don't place again.
    static PLACING: Cell<bool> = const { Cell::new(false) };
    /// Each AppKit class whose `-layout` Rune hooked, by class pointer, with the implementation
    /// the hook replaced: `None` when the class inherited `-layout` and the hook was added to it.
    static HOOKED: RefCell<HashMap<usize, Option<Imp>>> = RefCell::new(HashMap::new());
}

/// Drops its window's spot when the window goes, so a later window at the same address
/// doesn't inherit it.
pub(crate) struct Spot(usize);

impl Spot {
    pub(crate) fn new(window: &Window) -> Spot {
        Spot(ns_window(window).map_or(0, |w| Retained::as_ptr(&w) as usize))
    }
}

impl Drop for Spot {
    fn drop(&mut self) {
        let _ = SPOTS.try_with(|s| s.borrow_mut().remove(&self.0));
    }
}

/// Electron's `trafficLightPosition`: the titlebar container grows to the buttons' height
/// plus twice `y`, and the buttons sit `x, y` from the top-left at AppKit's own pitch.
pub(crate) fn place(window: &Window, (x, y): (f32, f32)) {
    let Some(win) = ns_window(window) else { return };
    let spot = (f64::from(x), f64::from(y));
    SPOTS.with(|s| s.borrow_mut().insert(Retained::as_ptr(&win) as usize, spot));
    place_in(&win, spot);
}

fn place_in(win: &NSWindow, (x, y): (f64, f64)) {
    if win.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return;
    }
    let buttons = [NSWindowButton::NSWindowCloseButton, NSWindowButton::NSWindowMiniaturizeButton, NSWindowButton::NSWindowZoomButton]
        .map(|b| win.standardWindowButton(b));
    let [Some(close), Some(mini), Some(zoom)] = buttons else { return };
    // SAFETY: AppKit's own view hierarchy, on the main thread.
    let Some(parent) = (unsafe { close.superview() }) else { return };
    let Some(container) = (unsafe { parent.superview() }) else { return };
    // SAFETY: live views on the main thread.
    unsafe {
        hold(&parent);
        hold(&container);
    }
    if PLACING.with(|p| p.replace(true)) {
        return;
    }
    // A right-to-left app's titlebar lays out from the right (`labels::set_direction`), as AppKit
    // lays out a right-to-left localization's: `x` in from the right edge, Close outermost (on macOS
    // 27 a native window's Close sits at 687-701 of 720 points, against 19-33 left to right). AppKit's
    // own `windowTitlebarLayoutDirection` follows the localization and has no setter, so Rune places
    // them, as it does the spot.
    let rtl = crate::labels::rtl();
    let pitch = (mini.frame().origin.x - close.frame().origin.x).abs();
    let mut frame = container.frame();
    frame.size.height = close.frame().size.height + 2.0 * y;
    frame.origin.y = win.frame().size.height - frame.size.height;
    let width = parent.frame().size.width;
    unsafe {
        container.setFrame(frame);
        for (i, b) in [close, mini, zoom].iter().enumerate() {
            let at = x + i as f64 * pitch;
            let at = if rtl { width - at - b.frame().size.width } else { at };
            b.setFrameOrigin(NSPoint::new(at, y));
        }
    }
    PLACING.with(|p| p.set(false));
}

/// Runs AppKit's `-layout` for the view's class, then puts the buttons back.
unsafe extern "C" fn layout(view: &NSView, sel: Sel) {
    if let Some(imp) = original(own_class(view)) {
        // SAFETY: `imp` is AppKit's -[NSView layout] implementation for this class chain, whose
        // signature is `(id, SEL) -> void`.
        unsafe {
            let f: unsafe extern "C" fn(&NSView, Sel) = std::mem::transmute(imp);
            f(view, sel);
        }
    }
    if PLACING.with(Cell::get) {
        return;
    }
    let Some(win) = view.window() else { return };
    let Some(spot) = SPOTS.with(|s| s.borrow().get(&(Retained::as_ptr(&win) as usize)).copied()) else { return };
    place_in(&win, spot);
}

/// Hooks `-layout` on this view's AppKit class, once per class. A class that inherits `-layout`
/// gets the hook added to it alone; one that defines it has its implementation swapped and kept.
/// Every view of the class then runs the hook, and `layout` leaves a window without a spot as
/// AppKit laid it out.
unsafe fn hold(view: &AnyObject) {
    let class = own_class(view);
    let key = class as *const AnyClass as usize;
    if HOOKED.with(|h| h.borrow().contains_key(&key)) {
        return;
    }
    let sel = sel!(layout);
    // SAFETY: the signature matches -[NSView layout]; `Imp` is the untyped function pointer.
    let hook: Imp = unsafe { std::mem::transmute(layout as unsafe extern "C" fn(&NSView, Sel)) };
    let replaced = if defines(class, sel) {
        let Some(method) = class.instance_method(sel) else { return };
        // SAFETY: same signature; the old implementation is kept and called by the hook.
        Some(unsafe { method.set_implementation(hook) })
    } else {
        let types: &CStr = c"v16@0:8";
        // SAFETY: a registered class, a matching type encoding, on the main thread.
        let added = unsafe {
            objc2::ffi::class_addMethod(class as *const AnyClass as *mut objc2::ffi::objc_class, sel.as_ptr(), Some(hook), types.as_ptr())
        };
        if added == objc2::ffi::NO {
            return;
        }
        None
    };
    HOOKED.with(|h| h.borrow_mut().insert(key, replaced));
}

/// The view's AppKit class, under the runtime subclass KVO puts above an observed view.
fn own_class(view: &AnyObject) -> &'static AnyClass {
    let mut class = view.class();
    while class.name().starts_with("NSKVONotifying_") {
        match class.superclass() {
            Some(superclass) => class = superclass,
            None => break,
        }
    }
    class
}

/// Whether `class` has its own `sel`, not one it inherits.
fn defines(class: &AnyClass, sel: Sel) -> bool {
    let Some(own) = class.instance_method(sel) else { return false };
    class.superclass().and_then(|s| s.instance_method(sel)).is_none_or(|inherited| !std::ptr::eq(own, inherited))
}

/// AppKit's own `-layout` for `class`: the implementation a hook replaced, or the nearest one up
/// the chain that is not a hook.
fn original(class: &AnyClass) -> Option<Imp> {
    let sel = sel!(layout);
    let mut class = class;
    loop {
        let key = class as *const AnyClass as usize;
        match HOOKED.with(|h| h.borrow().get(&key).copied()) {
            Some(Some(imp)) => return Some(imp),
            Some(None) => {}
            None if defines(class, sel) => return class.instance_method(sel).map(|m| m.implementation()),
            None => {}
        }
        class = class.superclass()?;
    }
}

#[cfg(test)]
#[path = "traffic_lights_tests.rs"]
mod tests;
