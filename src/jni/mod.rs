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
use jni::objects::{JByteArray, JClass, JObject, JString};
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
    let app = ctx.shell.app();
    app.on_camera_frame(
        &luma,
        width.max(0) as usize,
        height.max(0) as usize,
        row_stride.max(0) as usize,
        rotation.max(0) as u32,
    );

    // Every 30th frame, roughly once a second. Enough to see what the decoder is concluding and
    // how long it is taking without flooding logcat at frame rate.
    if app.frames() % 30 == 0 {
        info!(
            "frame {} ({} ms): {}",
            app.frames(),
            app.last_decode_ms(),
            app.status_line()
        );
    }
}

/// Report one physical lens the shim enumerated.
///
/// Called once per lens before streaming starts. Kotlin owns enumeration because only it can read
/// `CameraCharacteristics`; everything after that — annotating each lens with what it would deliver,
/// laying out the picker, resolving a tap — is Rust.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeAddLens(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
    id: JString<'_>,
    label: JString<'_>,
    focal_length_mm: jfloat,
    sensor_width_mm: jfloat,
    pixel_width: jint,
    min_focus_distance_m: jfloat,
) {
    let Some(ctx) = context(ptr) else {
        return;
    };
    let (Ok(id), Ok(label)) = (env.get_string(&id), env.get_string(&label)) else {
        error!("lens id/label not readable");
        return;
    };
    let spec = crate::camera::LensSpec {
        id: id.to_string_lossy().into_owned(),
        label: label.to_string_lossy().into_owned(),
        focal_length_mm,
        sensor_width_mm,
        pixel_width: pixel_width.max(0) as u32,
        min_focus_distance_m,
    };
    info!(
        "lens {} ({}): {:.2}mm on {:.2}mm sensor",
        spec.id, spec.label, spec.focal_length_mm, spec.sensor_width_mm
    );

    let app = ctx.shell.app();
    let mut lenses: Vec<crate::camera::LensSpec> = app.lenses().to_vec();
    lenses.push(spec);
    let current = app.current_lens().to_string();
    app.set_lenses(lenses, &current);
}

/// Tell Rust which physical camera is now streaming.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativeSetCurrentLens(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    ptr: jlong,
    id: JString<'_>,
) {
    let Some(ctx) = context(ptr) else {
        return;
    };
    if let Ok(id) = env.get_string(&id) {
        ctx.shell.app().set_current_lens(&id.to_string_lossy());
    }
}

/// Per-frame poll for a lens the user tapped.
///
/// A poll rather than a callback because reconfiguring the capture session has to happen on the
/// camera thread, and calling back into Java from inside a render pass to get there is a deadlock
/// waiting to happen. Returns `null` when nothing was requested.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativePollLensRequest<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) -> jni::sys::jstring {
    let Some(ctx) = context(ptr) else {
        return std::ptr::null_mut();
    };
    match ctx.shell.app().take_lens_request() {
        Some(id) => env
            .new_string(id)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

/// Per-frame poll for a PNG the user asked to save.
///
/// Rust encodes; Kotlin writes. The split is deliberate — only the platform layer can reach
/// MediaStore, and only Rust knows what the pristine symbol looks like. Returns `null` when nothing
/// is pending.
#[unsafe(no_mangle)]
pub extern "C" fn Java_com_barclean_BarcleanActivity_nativePollSaveRequest<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    ptr: jlong,
) -> jni::sys::jbyteArray {
    let Some(ctx) = context(ptr) else {
        return std::ptr::null_mut();
    };
    match ctx.shell.app().take_save_request() {
        Some(png) => {
            info!("save requested: {} byte PNG", png.len());
            let signed: Vec<i8> = png.into_iter().map(|b| b as i8).collect();
            env.byte_array_from_slice(unsafe {
                std::slice::from_raw_parts(signed.as_ptr() as *const u8, signed.len())
            })
            .map(|a| a.into_raw())
            .unwrap_or(std::ptr::null_mut())
        }
        None => std::ptr::null_mut(),
    }
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
