package org.personal.hopspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Rect
import android.os.BatteryManager
import android.view.GestureDetector
import android.view.MotionEvent
import android.view.View
import java.nio.ByteBuffer

/** Pixel OLED-style face used by the `oled` product flavor. */
class HopspotView(
    context: Context,
) : View(context) {
    private val bitmap = Bitmap.createBitmap(
        NativeBridge.PANEL_WIDTH,
        NativeBridge.PANEL_HEIGHT,
        Bitmap.Config.ARGB_8888,
    )
    private val buffer = ByteBuffer.allocateDirect(NativeBridge.RGBA_BYTES)
    private val paint = Paint(Paint.FILTER_BITMAP_FLAG).apply {
        isFilterBitmap = false
        isDither = false
    }
    private val src = Rect(0, 0, NativeBridge.PANEL_WIDTH, NativeBridge.PANEL_HEIGHT)
    private val dst = Rect()
    private val detector = GestureDetector(
        context,
        object : GestureDetector.SimpleOnGestureListener() {
            override fun onDown(e: MotionEvent): Boolean = true

            override fun onSingleTapUp(e: MotionEvent): Boolean {
                act(service?.postInput(NativeBridge.INPUT_SHORT_PRESS) ?: NativeBridge.ACTION_NONE)
                invalidate()
                return true
            }

            override fun onLongPress(e: MotionEvent) {
                act(service?.postInput(NativeBridge.INPUT_LONG_PRESS) ?: NativeBridge.ACTION_NONE)
                invalidate()
            }

            private fun act(action: Int) {
                if (action == NativeBridge.ACTION_ANNOUNCE) {
                    service?.announce()
                }
            }
        },
    )
    private val ticker = object : Runnable {
        override fun run() {
            invalidate()
            postDelayed(this, NativeBridge.RENDER_INTERVAL_MILLIS)
        }
    }

    init {
        setBackgroundColor(Color.BLACK)
        post(ticker)
    }

    fun setService(service: PrnsService?) {
        this.service = service
        invalidate()
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        val current = service
        if (current == null) {
            canvas.drawColor(Color.BLACK)
            return
        }
        if (batteryThrottle == 0) {
            pushBattery(current)
        }
        batteryThrottle = (batteryThrottle + 1) % BATTERY_EVERY_FRAMES
        current.render(buffer)
        buffer.rewind()
        bitmap.copyPixelsFromBuffer(buffer)
        buffer.rewind()

        val scale = minOf(
            width.toFloat() / NativeBridge.PANEL_WIDTH.toFloat(),
            height.toFloat() / NativeBridge.PANEL_HEIGHT.toFloat(),
        )
        val outWidth = (NativeBridge.PANEL_WIDTH * scale).toInt()
        val outHeight = (NativeBridge.PANEL_HEIGHT * scale).toInt()
        val left = (width - outWidth) / 2
        val top = (height - outHeight) / 2
        dst.set(left, top, left + outWidth, top + outHeight)
        canvas.drawBitmap(bitmap, src, dst, paint)
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        return detector.onTouchEvent(event) || super.onTouchEvent(event)
    }

    private fun pushBattery(current: PrnsService) {
        val status = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
            ?: return
        val level = status.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val scale = status.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
        if (level < 0 || scale <= 0) {
            return
        }
        val percent = level * 100 / scale
        val charging = status.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) != 0
        current.setBattery(percent, charging)
    }

    fun stop() {
        removeCallbacks(ticker)
    }

    private var batteryThrottle = 0
    private var service: PrnsService? = null

    private companion object {
        private const val BATTERY_EVERY_FRAMES = 30
    }
}
