package dev.dioxus.main

import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.util.Log
import android.util.Size
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.TextView
import androidx.activity.OnBackPressedCallback
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.AspectRatioStrategy
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * A live viewfinder. Frames are kept small enough to read a code as they arrive.
 */
class CodeScan(private val activity: MainActivity) {
    private var overlay: FrameLayout? = null
    private var camera: ExecutorService? = null
    private var provider: ProcessCameraProvider? = null
    private var back: OnBackPressedCallback? = null
    private val busy = AtomicBoolean(false)
    private val closed = AtomicBoolean(false)

    fun isOpen(): Boolean = overlay != null

    fun start(onCode: (String) -> Unit, onUnavailable: () -> Unit) {
        if (overlay != null) {
            return
        }
        val previewView = PreviewView(activity)
        previewView.scaleType = PreviewView.ScaleType.FILL_CENTER
        previewView.layoutParams = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
        )
        val root = FrameLayout(activity)
        root.setBackgroundColor(Color.BLACK)
        root.isClickable = true
        root.addView(previewView)
        root.addView(FrameGuide(activity))
        root.addView(label("←", Gravity.START or Gravity.TOP) { cancel() })
        root.addView(label("Point at their code", Gravity.BOTTOM or Gravity.CENTER_HORIZONTAL, null))
        val content = activity.findViewById<ViewGroup>(android.R.id.content)
        content.addView(
            root,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT,
            ),
        )
        overlay = root
        val callback = object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                cancel()
            }
        }
        activity.onBackPressedDispatcher.addCallback(activity, callback)
        back = callback

        val analysisExecutor = Executors.newSingleThreadExecutor()
        camera = analysisExecutor
        val preview = Preview.Builder()
            .setResolutionSelector(fourByThree())
            .build()
        preview.surfaceProvider = previewView.surfaceProvider
        val analysis = ImageAnalysis.Builder()
            .setResolutionSelector(analysisSize())
            .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
            .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
            .build()
        analysis.setAnalyzer(analysisExecutor) { image ->
            readFrame(image, onCode)
        }
        val future = ProcessCameraProvider.getInstance(activity)
        future.addListener({
            if (closed.get()) {
                return@addListener
            }
            try {
                val cameras = future.get()
                provider = cameras
                val selector = when {
                    cameras.hasCamera(CameraSelector.DEFAULT_BACK_CAMERA) ->
                        CameraSelector.DEFAULT_BACK_CAMERA
                    cameras.hasCamera(CameraSelector.DEFAULT_FRONT_CAMERA) ->
                        CameraSelector.DEFAULT_FRONT_CAMERA
                    else -> {
                        closed.set(true)
                        close()
                        onUnavailable()
                        return@addListener
                    }
                }
                cameras.unbindAll()
                cameras.bindToLifecycle(activity, selector, preview, analysis)
                if (closed.get()) {
                    cameras.unbindAll()
                }
            } catch (error: Exception) {
                Log.e(TAG, "The camera didn't open", error)
                closed.set(true)
                close()
                onUnavailable()
            }
        }, ContextCompat.getMainExecutor(activity))
    }

    fun cancel() {
        if (closed.compareAndSet(false, true)) {
            close()
            activity.finishScan(null, MainActivity.CANCELLED)
        }
    }

    private fun readFrame(image: ImageProxy, onCode: (String) -> Unit) {
        if (closed.get() || !busy.compareAndSet(false, true)) {
            image.close()
            return
        }
        val luma = uprightLuma(image, ANALYSIS_EDGE)
        image.close()
        val text = if (luma == null) {
            null
        } else {
            nativeReadCode(luma.width, luma.height, luma.bytes)
        }
        if (text != null && closed.compareAndSet(false, true)) {
            activity.runOnUiThread {
                close()
                onCode(text)
            }
        }
        busy.set(false)
    }

    private fun close() {
        back?.remove()
        back = null
        provider?.unbindAll()
        provider = null
        camera?.shutdown()
        camera = null
        overlay?.let { view ->
            (view.parent as? ViewGroup)?.removeView(view)
        }
        overlay = null
    }

    private fun label(text: String, gravity: Int, onTap: (() -> Unit)?): TextView {
        val density = activity.resources.displayMetrics.density
        val view = TextView(activity)
        view.text = text
        view.setTextColor(Color.parseColor("#f3efe4"))
        view.textSize = 18f
        view.gravity = Gravity.CENTER
        val pad = (16 * density).toInt()
        view.setPadding(pad, (48 * density).toInt(), pad, (28 * density).toInt())
        view.layoutParams = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.WRAP_CONTENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
            gravity,
        )
        if (onTap != null) {
            view.setOnClickListener { onTap() }
        }
        return view
    }

    private class FrameGuide(context: android.content.Context) : View(context) {
        private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            color = Color.parseColor("#f3efe4")
            strokeWidth = 2f * resources.displayMetrics.density
        }

        override fun onDraw(canvas: Canvas) {
            val edge = 220f * resources.displayMetrics.density
            val left = (width - edge) / 2f
            val top = (height - edge) / 2f
            val corner = 12f * resources.displayMetrics.density
            canvas.drawRoundRect(left, top, left + edge, top + edge, corner, corner, paint)
        }
    }

    private data class Luma(val width: Int, val height: Int, val bytes: ByteArray)

    private companion object {
        private const val TAG = "Journal"
        private const val ANALYSIS_EDGE = 640

        @JvmStatic
        private external fun nativeReadCode(width: Int, height: Int, luma: ByteArray): String?

        private fun fourByThree(): ResolutionSelector {
            return ResolutionSelector.Builder()
                .setAspectRatioStrategy(AspectRatioStrategy.RATIO_4_3_FALLBACK_AUTO_STRATEGY)
                .build()
        }

        private fun analysisSize(): ResolutionSelector {
            return ResolutionSelector.Builder()
                .setResolutionStrategy(
                    ResolutionStrategy(
                        Size(640, 480),
                        ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER,
                    ),
                )
                .build()
        }

        private fun uprightLuma(image: ImageProxy, maxEdge: Int): Luma? {
            val plane = image.planes.firstOrNull() ?: return null
            val width = image.width
            val height = image.height
            if (width <= 0 || height <= 0) {
                return null
            }
            val longest = maxOf(width, height)
            val step = maxOf(1, (longest + maxEdge - 1) / maxEdge)
            val sampledWidth = (width + step - 1) / step
            val sampledHeight = (height + step - 1) / step
            val buffer = plane.buffer
            val rowStride = plane.rowStride
            val pixelStride = plane.pixelStride
            val sampled = ByteArray(sampledWidth * sampledHeight)
            var out = 0
            var y = 0
            while (y < height && out < sampled.size) {
                val row = y * rowStride
                var x = 0
                while (x < width && out < sampled.size) {
                    val index = row + x * pixelStride
                    sampled[out] = if (index >= 0 && index < buffer.limit()) buffer.get(index) else 0
                    out += 1
                    x += step
                }
                y += step
            }
            if (out != sampled.size) {
                return null
            }
            return rotate(sampled, sampledWidth, sampledHeight, image.imageInfo.rotationDegrees)
        }

        private fun rotate(source: ByteArray, width: Int, height: Int, degrees: Int): Luma {
            when (degrees) {
                90 -> {
                    val out = ByteArray(source.size)
                    var i = 0
                    for (y in 0 until height) {
                        for (x in 0 until width) {
                            val destX = height - 1 - y
                            val destY = x
                            out[destY * height + destX] = source[i]
                            i += 1
                        }
                    }
                    return Luma(height, width, out)
                }
                180 -> {
                    val out = ByteArray(source.size)
                    var i = 0
                    for (y in 0 until height) {
                        for (x in 0 until width) {
                            val destX = width - 1 - x
                            val destY = height - 1 - y
                            out[destY * width + destX] = source[i]
                            i += 1
                        }
                    }
                    return Luma(width, height, out)
                }
                270 -> {
                    val out = ByteArray(source.size)
                    var i = 0
                    for (y in 0 until height) {
                        for (x in 0 until width) {
                            val destX = y
                            val destY = width - 1 - x
                            out[destY * height + destX] = source[i]
                            i += 1
                        }
                    }
                    return Luma(height, width, out)
                }
                else -> return Luma(width, height, source)
            }
        }
    }
}
