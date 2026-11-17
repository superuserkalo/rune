use std::cell::Cell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ClassBuilder, NSObject};
use objc2::{msg_send, msg_send_id, ClassType};
use objc2_foundation::NSString;

use super::*;

thread_local! {
    /// Calls of the stand-in `-layout` on this test's thread, so parallel tests don't count each other's.
    static LAYOUTS: Cell<usize> = const { Cell::new(0) };
}

extern "C" fn counted_layout(_: &AnyObject, _: Sel) {
    LAYOUTS.with(|n| n.set(n.get() + 1));
}

fn layouts() -> usize {
    LAYOUTS.with(Cell::get)
}

extern "C" fn no_window(_: &AnyObject, _: Sel) -> *mut AnyObject {
    std::ptr::null_mut()
}

/// A stand-in for an AppKit view: `-layout` counts its calls and `-window` is nil, so the hook
/// runs the original and has no window to place buttons in.
fn view_class(name: &str) -> &'static AnyClass {
    let mut builder = ClassBuilder::new(name, NSObject::class()).expect("a fresh test class name");
    // SAFETY: the signatures match -layout and -window.
    unsafe {
        builder.add_method(sel!(layout), counted_layout as extern "C" fn(_, _));
        builder.add_method(sel!(window), no_window as extern "C" fn(_, _) -> _);
    }
    builder.register()
}

fn subclass(name: &str, base: &'static AnyClass) -> &'static AnyClass {
    ClassBuilder::new(name, base).expect("a fresh test class name").register()
}

fn new(class: &'static AnyClass) -> Retained<AnyObject> {
    // SAFETY: +new on a registered NSObject subclass.
    unsafe { msg_send_id![class, new] }
}

fn observe(object: &AnyObject, observer: &AnyObject) {
    let key = NSString::from_str("probe");
    // SAFETY: plain key-value observing on a live object, removed by `unobserve`.
    let _: () = unsafe {
        msg_send![object, addObserver: observer, forKeyPath: &*key, options: 0usize, context: std::ptr::null_mut::<c_void>()]
    };
}

fn unobserve(object: &AnyObject, observer: &AnyObject) {
    let key = NSString::from_str("probe");
    // SAFETY: removes the observer `observe` added.
    let _: () = unsafe { msg_send![object, removeObserver: observer, forKeyPath: &*key, context: std::ptr::null_mut::<c_void>()] };
}

fn layout_of(object: &AnyObject) {
    // SAFETY: the test classes implement -layout.
    let _: () = unsafe { msg_send![object, layout] };
}

#[test]
fn a_view_kvo_already_observes_keeps_kvo_on_top_and_its_observer_comes_off() {
    let class = view_class("RuneTrafficLightsTestObserved");
    let view = new(class);
    let observer = new(NSObject::class());
    observe(&view, &observer);
    assert!(view.class().name().starts_with("NSKVONotifying_"), "KVO subclassed the instance first");

    // SAFETY: a live object on this thread.
    unsafe { hold(&view) };

    assert!(view.class().name().starts_with("NSKVONotifying_"), "the instance keeps KVO's class");
    assert_eq!(own_class(&view).name(), class.name());
    let before = layouts();
    layout_of(&view);
    assert_eq!(layouts(), before + 1, "the hook runs the original -layout once");
    // With a per-instance subclass above KVO's, removing the observer read KVO's data from the
    // wrong class and jumped to null.
    unobserve(&view, &observer);
}

#[test]
fn a_class_that_inherits_layout_gets_the_hook_and_runs_its_superclass_layout() {
    let base = view_class("RuneTrafficLightsTestBase");
    let inherits = subclass("RuneTrafficLightsTestInherits", base);
    assert!(defines(base, sel!(layout)));
    assert!(!defines(inherits, sel!(layout)));

    let view = new(inherits);
    // SAFETY: a live object on this thread.
    unsafe { hold(&view) };
    assert!(defines(inherits, sel!(layout)), "the hook is added to the subclass alone");
    assert!(!HOOKED.with(|h| h.borrow().contains_key(&(base as *const AnyClass as usize))), "the superclass is untouched");

    let before = layouts();
    layout_of(&view);
    layout_of(&new(base));
    assert_eq!(layouts(), before + 2, "one original -layout per call, no recursion");
}

#[test]
fn holding_twice_hooks_a_class_once() {
    let class = view_class("RuneTrafficLightsTestTwice");
    let (a, b) = (new(class), new(class));
    // SAFETY: live objects on this thread.
    unsafe {
        hold(&a);
        hold(&b);
        hold(&a);
    }
    let before = layouts();
    layout_of(&a);
    assert_eq!(layouts(), before + 1, "the original runs once, not through a hook on a hook");
}
