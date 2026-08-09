//! JNI entry points matching `BarcleanActivity.kt`.
//!
//! Deliberately thin. Every one of these is a translation of an Android callback into a call on
//! fluor's [`AndroidShell`], which owns the app, the surface and the render pipeline. No logic
//! lives here, because logic here is logic that cannot be tested off-device.
//!
//! The Kotlin side owns exactly what Android will not let Rust own — the Activity lifecycle, the
//! Surface, runtime permissions, and Camera2 — and hands the results across. Everything else,
//! including all drawing and all decoding, is Rust.

use crate::app::BarcleanApp;
use fluor::host::android::AndroidShell;
use jni::objects::{JByteArray, JClass, JObject};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::JNIEnv;
use log::{error, info};
use ndk::native_window::NativeWindow;

/// Everything the Activity holds across its lifetime, behind one opaque `jlong`.
pub struct BarcleanContext {
    pub shell: AndroidShell<BarcleanApp>,
}

/// Recover the context from the pointer Kotlin is holding.
///
/// A zero or stale pointer is a no-op rather than a crash: Android will deliver callbacks during
/// teardown after `nativeDestroy` has run, and taking the app down over a late frame callback would
/// be a self-inflicted crash report.
fn context(ptr: jlong) -> Option<&'static mut BarcleanContext> {
    if ptr == 0 {
        return None;
    }
    Some(unsafe { &mut *(ptr as *mut BarcleanContext) })
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeInit(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    width: jint,
    height: jint,
) -> jlong {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("barclean"),
    );
    std::panic::set_hook(Box::new(|info| {
        error!("PANIC: {info}");
    }));

    info!("nativeInit {width}x{height}");
    let context = Box::new(BarcleanContext {
        shell: AndroidShell::new(BarcleanApp::new(), width.max(1) as u32, height.max(1) as u32),
    });
    Box::into_raw(context) as jlong
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeDraw(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
    surface: JObject<'_>,
) {
    let Some(ctx) = context(ptr) else {
        return;
    };
    let Some(window) = (unsafe { NativeWindow::from_surface(env.get_raw(), surface.as_raw()) })
    else {
        error!("Surface could not be converted to a NativeWindow");
        return;
    };
    ctx.shell.draw(&window);
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeResize(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
    width: jint,
    height: jint,
) {
    if let Some(ctx) = context(ptr) {
        ctx.shell.resize(width.max(1) as u32, height.max(1) as u32);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeOnTouch(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
    action: jint,
    x: jfloat,
    y: jfloat,
) -> jint {
    context(ptr).map_or(0, |ctx| ctx.shell.on_touch(action, x, y))
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeOnBackPressed(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
) -> jboolean {
    context(ptr).is_some_and(|ctx| ctx.shell.on_back_pressed()) as jboolean
}

/// Hand one camera frame's luminance plane to the app.
///
/// Y plane only. `YUV_420_888` gives it to us already separated, so there is no colour conversion
/// and no copy of the chroma planes we are not using yet — a barcode is a structure in luminance,
/// and the chroma planes matter only once the confidence sampler needs its neutrality signal.
///
/// `row_stride` is **not** always equal to `width`: Camera2 pads rows to a hardware alignment, and
/// assuming otherwise produces a picture that shears progressively down the frame. It is passed
/// explicitly rather than inferred for exactly that reason.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeOnCameraFrame(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
    luma: JByteArray<'_>,
    width: jint,
    height: jint,
    row_stride: jint,
    rotation: jint,
) {
    let Some(ctx) = context(ptr) else {
        return;
    };
    let Ok(bytes) = env.convert_byte_array(&luma) else {
        error!("could not read camera luma plane");
        return;
    };
    // JNI hands back i8; the plane is unsigned.
    let luma: Vec<u8> = bytes.into_iter().map(|b| b as u8).collect();
    ctx.shell.app().on_camera_frame(
        &luma,
        width.max(0) as usize,
        height.max(0) as usize,
        row_stride.max(0) as usize,
        rotation.max(0) as u32,
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeDestroy(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
) {
    if ptr == 0 {
        return;
    }
    info!("nativeDestroy");
    drop(unsafe { Box::from_raw(ptr as *mut BarcleanContext) });
}
