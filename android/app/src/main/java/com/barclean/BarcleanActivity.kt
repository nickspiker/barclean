package com.barclean

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.graphics.ImageFormat
import android.graphics.PixelFormat
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
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

    private val frameCallback = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (nativePtr != 0L && surfaceReady) {
                val holder = surfaceView.holder
                if (holder.surface.isValid) {
                    nativeDraw(nativePtr, holder.surface)
                }
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
     * Enumerate the physical lenses behind a logical multi-camera.
     *
     * A modern phone exposes one logical camera that fronts three or four physical modules with
     * genuinely different angular resolutions. Choosing between them is barclean's whole camera
     * story, so the specs are read here and handed to Rust, which annotates them for the picker.
     * Reported but not yet wired to a control — that is the next step.
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

        val ids = if (physicalIds.isEmpty()) setOf(logicalId) else physicalIds
        for (id in ids) {
            try {
                val c = manager.getCameraCharacteristics(id)
                val focal = c.get(CameraCharacteristics.LENS_INFO_AVAILABLE_FOCAL_LENGTHS)
                val size = c.get(CameraCharacteristics.SENSOR_INFO_PHYSICAL_SIZE)
                val minFocus = c.get(CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE)
                Log.i(
                    TAG,
                    "lens $id: focal=${focal?.joinToString()} sensor=${size?.width}x${size?.height}mm " +
                        "minFocusDiopters=$minFocus"
                )
            } catch (e: Throwable) {
                Log.w(TAG, "lens $id unreadable: ${e.message}")
            }
        }
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
        }

        @Suppress("DEPRECATION")
        camera.createCaptureSession(
            listOf(reader.surface),
            object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(session: CameraCaptureSession) {
                    captureSession = session
                    session.setRepeatingRequest(request.build(), null, cameraHandler)
                    Log.i(TAG, "capture session running")
                }

                override fun onConfigureFailed(session: CameraCaptureSession) {
                    Log.e(TAG, "capture session configuration failed")
                }
            },
            cameraHandler
        )
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
