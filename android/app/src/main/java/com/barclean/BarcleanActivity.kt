package com.barclean

import android.Manifest
import android.app.Activity
import android.content.ContentValues
import android.content.pm.PackageManager
import android.os.Environment
import android.provider.MediaStore
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import android.graphics.ImageFormat
import android.graphics.PixelFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.params.OutputConfiguration
import android.hardware.camera2.params.SessionConfiguration
import android.media.ImageReader
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.Choreographer
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager

/**
 * The entire Android surface area of barclean.
 *
 * Everything Android insists on owning lives here — the Activity lifecycle, the Surface, runtime
 * permissions, and Camera2 — and nothing else does. All drawing and all decoding happen in Rust,
 * reached through the `native*` methods below. The rule is that anything implemented in Kotlin is
 * something that cannot be tested without a phone plugged in, so there should be as little of it as
 * possible.
 */
class BarcleanActivity : Activity(), SurfaceHolder.Callback {

    companion object {
        private const val TAG = "barclean"
        private const val CAMERA_PERMISSION_REQUEST = 1

        init {
            System.loadLibrary("barclean")
        }
    }

    private external fun nativeInit(width: Int, height: Int): Long
    private external fun nativeDraw(ptr: Long, surface: Surface)
    private external fun nativeResize(ptr: Long, width: Int, height: Int)
    private external fun nativeOnTouch(ptr: Long, action: Int, x: Float, y: Float): Int
    private external fun nativeOnBackPressed(ptr: Long): Boolean
    private external fun nativeOnCameraFrame(
        ptr: Long,
        luma: ByteArray,
        width: Int,
        height: Int,
        rowStride: Int,
        rotationDegrees: Int
    )
    private external fun nativeAddLens(
        ptr: Long,
        id: String,
        label: String,
        focalLengthMm: Float,
        sensorWidthMm: Float,
        pixelWidth: Int,
        minFocusDistanceM: Float
    )
    private external fun nativeSetCurrentLens(ptr: Long, id: String)
    private external fun nativePollLensRequest(ptr: Long): String?
    private external fun nativePollSaveRequest(ptr: Long): ByteArray?
    private external fun nativeDestroy(ptr: Long)

    private var nativePtr = 0L
    private lateinit var surfaceView: SurfaceView
    private var surfaceReady = false

    private var cameraDevice: CameraDevice? = null
    private var captureSession: CameraCaptureSession? = null
    private var imageReader: ImageReader? = null
    private var cameraThread: HandlerThread? = null
    private var cameraHandler: Handler? = null

    /** Reused across frames so the capture path does not allocate a multi-megabyte array at 30 Hz. */
    private var lumaBuffer: ByteArray? = null

    /**
     * SENSOR_ORIENTATION: clockwise degrees needed to bring the sensor's output upright.
     *
     * Phone image sensors are mounted landscape regardless of how the phone is held, so this is 90
     * on essentially every device in portrait. Rust rotates at sample time rather than rotating the
     * buffer, so this only has to be reported, never applied here.
     */
    private var sensorOrientation = 0

    /** Physical camera ids behind the logical camera, widest first. */
    private var physicalLensIds: List<String> = emptyList()

    /** The physical lens currently streaming. */
    private var currentLensId: String = ""

    private val frameCallback = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (nativePtr != 0L && surfaceReady) {
                val holder = surfaceView.holder
                if (holder.surface.isValid) {
                    nativeDraw(nativePtr, holder.surface)
                }
                nativePollLensRequest(nativePtr)?.let { selectPhysicalLens(it) }
                nativePollSaveRequest(nativePtr)?.let { savePng(it) }
            }
            Choreographer.getInstance().postFrameCallback(this)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        surfaceView = SurfaceView(this)
        // Load-bearing, and fatal to omit. fluor's Android present path locks the ANativeWindow and
        // treats `bits` as a `*mut u32` slice of `stride * height` — i.e. it assumes 32 bits per
        // pixel — and it never calls ANativeWindow_setBuffersGeometry, so the format is entirely
        // this side's responsibility. Left unset, the surface can hand back a narrower buffer and
        // the finalize pass writes past the real allocation, taking SIGSEGV/SEGV_ACCERR deep inside
        // fluor where it looks like a fluor bug rather than a missing line here.
        surfaceView.holder.setFormat(PixelFormat.RGBA_8888)
        surfaceView.holder.addCallback(this)
        setContentView(surfaceView)

        if (checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), CAMERA_PERMISSION_REQUEST)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == CAMERA_PERMISSION_REQUEST &&
            grantResults.isNotEmpty() &&
            grantResults[0] == PackageManager.PERMISSION_GRANTED
        ) {
            if (surfaceReady) openCamera()
        } else {
            Log.w(TAG, "camera permission denied; nothing to decode")
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surfaceReady = true
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        if (nativePtr == 0L) {
            nativePtr = nativeInit(width, height)
            Log.i(TAG, "native context ${width}x$height -> 0x${nativePtr.toString(16)}")
            Choreographer.getInstance().postFrameCallback(frameCallback)
        } else {
            nativeResize(nativePtr, width, height)
        }
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            openCamera()
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surfaceReady = false
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (nativePtr == 0L) return super.onTouchEvent(event)
        nativeOnTouch(nativePtr, event.actionMasked, event.x, event.y)
        return true
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (nativePtr != 0L && nativeOnBackPressed(nativePtr)) return
        @Suppress("DEPRECATION")
        super.onBackPressed()
    }

    override fun onPause() {
        super.onPause()
        closeCamera()
    }

    override fun onResume() {
        super.onResume()
        if (surfaceReady &&
            checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
        ) {
            openCamera()
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        Choreographer.getInstance().removeFrameCallback(frameCallback)
        closeCamera()
        if (nativePtr != 0L) {
            nativeDestroy(nativePtr)
            nativePtr = 0L
        }
    }

    /**
     * Write a cleaned symbol to the device's picture library.
     *
     * MediaStore rather than app-private storage, because the entire point of the export is to use
     * the file somewhere else — it should land in Photos alongside everything else, not somewhere
     * only this app can reach.
     *
     * Named for the moment of capture, in local time, as `2016-08-10 14:33:48.png`.
     */
    private fun savePng(png: ByteArray) {
        val name = SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US).format(Date()) + ".png"
        try {
            val values = ContentValues().apply {
                put(MediaStore.Images.Media.DISPLAY_NAME, name)
                put(MediaStore.Images.Media.MIME_TYPE, "image/png")
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    put(
                        MediaStore.Images.Media.RELATIVE_PATH,
                        Environment.DIRECTORY_PICTURES + "/barclean"
                    )
                }
            }
            val uri = contentResolver.insert(
                MediaStore.Images.Media.EXTERNAL_CONTENT_URI,
                values
            ) ?: run {
                Log.e(TAG, "MediaStore refused an entry for $name")
                return
            }
            contentResolver.openOutputStream(uri)?.use { it.write(png) }
            Log.i(TAG, "saved $name (${png.size} bytes) -> $uri")
        } catch (e: Throwable) {
            Log.e(TAG, "save failed for $name", e)
        }
    }

    /**
     * Enumerate the physical lenses behind a logical multi-camera and report them to Rust.
     *
     * A modern phone exposes one logical camera fronting three or four physical modules with
     * genuinely different angular resolutions. Only Kotlin can read `CameraCharacteristics`, so
     * enumeration happens here — everything after that (predicting what each lens would deliver,
     * laying out the picker, resolving a tap) is Rust.
     *
     * `LENS_INFO_MINIMUM_FOCUS_DISTANCE` is reported in **diopters**, not metres: 10.0 means 10 cm,
     * and 0.0 means fixed-focus rather than "focuses at zero distance". Reading it as metres would
     * mark every lens as capable of focusing anywhere.
     */
    private fun describeLenses(manager: CameraManager, logicalId: String) {
        val characteristics = manager.getCameraCharacteristics(logicalId)
        val physicalIds = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            try {
                characteristics.physicalCameraIds
            } catch (e: Throwable) {
                emptySet<String>()
            }
        } else {
            emptySet()
        }

        val ids = (if (physicalIds.isEmpty()) setOf(logicalId) else physicalIds).toList()
        physicalLensIds = ids

        // Named so the magnification lambdas below can take it as a parameter type.
        data class Lens(val id: String, val focal: Float, val sensorW: Float, val minFocusM: Float)
        val lenses = mutableListOf<Lens>()
        for (id in ids) {
            try {
                val c = manager.getCameraCharacteristics(id)
                val focal = c.get(CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS)
                    ?.firstOrNull() ?: continue
                val size = c.get(CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE) ?: continue
                val diopters = c.get(CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
                val minFocusM = if (diopters > 0f) 1f / diopters else 0f
                lenses.add(Lens(id, focal, size.width, minFocusM))
            } catch (e: Throwable) {
                Log.w(TAG, "lens $id unreadable: ${e.message}")
            }
        }

        // Collapse cropped-mode duplicates. The Pixel exposes each module twice — once at full
        // sensor and once as a cropped sub-frame with the same focal length — which would show the
        // user two identical "1x" buttons. Keep the larger sensor of each focal length, since that
        // is the one with the pixels.
        val deduped = lenses
            .groupBy { String.format("%.2f", it.focal) }
            .values
            .map { group -> group.maxByOrNull { it.sensorW }!! }

        // Label by ANGULAR magnification, not focal length. The three modules have different
        // physical sensor widths, so focal length alone does not say how much of the scene a lens
        // takes in: half the field of view is atan(sensorWidth / 2f), which makes the figure of
        // merit f/sensorWidth. Using bare focal ratios labelled this phone's 0.5x ultra-wide as
        // "0.3x" and its 5x telephoto as "3x", because their sensors are smaller than the main
        // camera's and that shrinks their field of view further than focal length suggests.
        //
        // Zoom is also not linear in degrees — it scales with the tangent of the half-angle — but
        // that falls out of the f/w ratio automatically, since tan(hfov/2) IS w/2f.
        val power = { l: Lens -> l.focal / l.sensorW }
        val widest = deduped.minByOrNull(power)
        val main = deduped.filter { power(it) > power(widest!!) * 1.5f }.minByOrNull(power)
            ?: widest
        val mainPower = power(main!!)
        for (l in deduped.sortedBy(power)) {
            val ratio = power(l) / mainPower
            val label = when {
                ratio < 0.95f -> String.format("%.1fx", ratio)
                ratio < 1.25f -> "1x"
                ratio < 9.5f -> String.format("%.1fx", ratio).removeSuffix(".0x").let {
                    if (it.endsWith("x")) it else it + "x"
                }
                else -> String.format("%.0fx", ratio)
            }
            nativeAddLens(nativePtr, l.id, label, l.focal, l.sensorW, 1280, l.minFocusM)
        }
    }

    /**
     * Switch the capture session to a physical lens the user picked.
     *
     * Rebuilds the request with `setPhysicalCameraId` rather than reopening the camera, which keeps
     * the switch fast enough to feel like a button press instead of a restart.
     */
    private fun selectPhysicalLens(id: String) {
        if (id == currentLensId) return
        val camera = cameraDevice ?: return
        val reader = imageReader ?: return
        currentLensId = id
        nativeSetCurrentLens(nativePtr, id)
        // A physical camera is chosen on the OUTPUT, not the request: setPhysicalCameraId lives on
        // OutputConfiguration, so switching lenses means rebuilding the capture session rather than
        // swapping a request field. Costs a session teardown, which is why the picker is a
        // deliberate tap rather than anything automatic.
        captureSession?.close()
        captureSession = null
        startSession(camera, reader)
    }

    private fun openCamera() {
        if (cameraDevice != null) return
        val manager = getSystemService(CAMERA_SERVICE) as CameraManager

        val cameraId = manager.cameraIdList.firstOrNull { id ->
            manager.getCameraCharacteristics(id)
                .get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
        } ?: manager.cameraIdList.firstOrNull() ?: run {
            Log.e(TAG, "no cameras")
            return
        }

        sensorOrientation = manager.getCameraCharacteristics(cameraId)
            .get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0
        Log.i(TAG, "sensor orientation ${sensorOrientation}deg")

        describeLenses(manager, cameraId)
        currentLensId = physicalLensIds.firstOrNull { it != cameraId } ?: cameraId
        // Default to the main camera when one is identifiable, matching what every camera app opens
        // on — the widest is rarely the right first choice for reading something.
        currentLensId = physicalLensIds.getOrNull(1) ?: physicalLensIds.firstOrNull() ?: cameraId
        nativeSetCurrentLens(nativePtr, currentLensId)

        cameraThread = HandlerThread("barclean-camera").also { it.start() }
        cameraHandler = Handler(cameraThread!!.looper)

        // 1280x720 preview: enough pixels per module for a symbol filling a reasonable part of the
        // frame, small enough that a full decode attempt keeps up with the frame rate. YUV_420_888
        // hands the luminance plane over already separated, so there is no colour conversion on the
        // hot path — a barcode is a structure in luminance.
        val reader = ImageReader.newInstance(1280, 720, ImageFormat.YUV_420_888, 2)
        reader.setOnImageAvailableListener({ r ->
            val image = r.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                if (nativePtr != 0L) {
                    val plane = image.planes[0]
                    val buffer = plane.buffer
                    val needed = buffer.remaining()
                    var buf = lumaBuffer
                    if (buf == null || buf.size < needed) {
                        buf = ByteArray(needed)
                        lumaBuffer = buf
                    }
                    buffer.get(buf, 0, needed)
                    nativeOnCameraFrame(
                        nativePtr,
                        buf,
                        image.width,
                        image.height,
                        plane.rowStride,
                        sensorOrientation
                    )
                }
            } catch (e: Throwable) {
                Log.e(TAG, "frame handling failed", e)
            } finally {
                image.close()
            }
        }, cameraHandler)
        imageReader = reader

        try {
            manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
                override fun onOpened(camera: CameraDevice) {
                    cameraDevice = camera
                    startSession(camera, reader)
                }

                override fun onDisconnected(camera: CameraDevice) {
                    camera.close()
                    cameraDevice = null
                }

                override fun onError(camera: CameraDevice, error: Int) {
                    Log.e(TAG, "camera error $error")
                    camera.close()
                    cameraDevice = null
                }
            }, cameraHandler)
        } catch (e: SecurityException) {
            Log.e(TAG, "camera permission missing at open", e)
        }
    }

    private fun startSession(camera: CameraDevice, reader: ImageReader) {
        val request = camera.createCaptureRequest(CameraDevice.TEMPLATE_PREVIEW).apply {
            addTarget(reader.surface)
            // Auto everything, which is the opposite of Lumis's stance and correct here: the goal
            // is a legible symbol, not untouched photons.
            set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE)
            set(CaptureRequest.CONTROL_AE_MODE, CaptureRequest.CONTROL_AE_MODE_ON)
        }.build()

        val callback = object : CameraCaptureSession.StateCallback() {
            override fun onConfigured(session: CameraCaptureSession) {
                captureSession = session
                session.setRepeatingRequest(request, null, cameraHandler)
                Log.i(TAG, "capture session running on lens $currentLensId")
            }

            override fun onConfigureFailed(session: CameraCaptureSession) {
                Log.e(TAG, "capture session configuration failed for lens $currentLensId")
            }
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P && currentLensId.isNotEmpty()) {
            val output = OutputConfiguration(reader.surface).apply {
                // Only route to a physical id when it is genuinely one of the physical cameras;
                // handing the logical id here fails configuration on some HALs.
                if (physicalLensIds.contains(currentLensId)) {
                    setPhysicalCameraId(currentLensId)
                }
            }
            camera.createCaptureSession(
                SessionConfiguration(
                    SessionConfiguration.SESSION_REGULAR,
                    listOf(output),
                    mainExecutor,
                    callback
                )
            )
        } else {
            @Suppress("DEPRECATION")
            camera.createCaptureSession(listOf(reader.surface), callback, cameraHandler)
        }
    }

    private fun closeCamera() {
        captureSession?.close()
        captureSession = null
        cameraDevice?.close()
        cameraDevice = null
        imageReader?.close()
        imageReader = null
        cameraThread?.quitSafely()
        cameraThread = null
        cameraHandler = null
    }
}
