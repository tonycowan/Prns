package dev.dioxus.main

import android.graphics.ImageFormat
import android.graphics.Rect
import android.graphics.YuvImage
import android.media.Image
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.util.Log
import android.util.Size
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.core.content.ContextCompat
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The camera for a video call. Frames stay small enough to share on the link.
 */
class Film(private val activity: MainActivity) {
    private var provider: ProcessCameraProvider? = null
    private var camera: ExecutorService? = null
    private var encoder: MediaCodec? = null
    private var decoder: MediaCodec? = null
    private val codecs = Any()
    private val closed = AtomicBoolean(false)
    private var semiplanar = true
    private var stride = WIDTH
    private var slice = HEIGHT
    private var pictureW = 0
    private var pictureH = 0
    private var decoderW = 0
    private var decoderH = 0
    private var presented = 0L
    private var decoded = 0L
    private var lastFrame = 0L

    fun start(onReady: (Boolean) -> Unit) {
        val executor = Executors.newSingleThreadExecutor()
        camera = executor
        val analysis = ImageAnalysis.Builder()
            .setResolutionSelector(frameSize())
            .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
            .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
            .build()
        analysis.setAnalyzer(executor) { image -> onFrame(image) }
        val future = ProcessCameraProvider.getInstance(activity)
        future.addListener({
            if (closed.get()) {
                onReady(false)
                return@addListener
            }
            try {
                val cameras = future.get()
                provider = cameras
                val selector = when {
                    cameras.hasCamera(CameraSelector.DEFAULT_FRONT_CAMERA) ->
                        CameraSelector.DEFAULT_FRONT_CAMERA
                    cameras.hasCamera(CameraSelector.DEFAULT_BACK_CAMERA) ->
                        CameraSelector.DEFAULT_BACK_CAMERA
                    else -> {
                        closed.set(true)
                        releaseCodecs()
                        onReady(false)
                        return@addListener
                    }
                }
                cameras.unbindAll()
                cameras.bindToLifecycle(activity, selector, analysis)
                if (closed.get()) {
                    cameras.unbindAll()
                    onReady(false)
                } else {
                    onReady(true)
                }
            } catch (error: Exception) {
                Log.e(TAG, "The camera didn't open", error)
                closed.set(true)
                releaseCodecs()
                onReady(false)
            }
        }, ContextCompat.getMainExecutor(activity))
    }

    fun stop() {
        note("stop enter")
        closed.set(true)
        synchronized(codecs) {
            provider?.unbindAll()
            provider = null
            camera?.shutdown()
            camera = null
            releaseCodecs()
        }
        note("stop leave")
    }

    fun decode(frame: ByteArray): ByteArray? {
        synchronized(codecs) {
            return decodeLocked(frame)
        }
    }
    private fun decodeLocked(frame: ByteArray): ByteArray? {
        note("decode enter ${frame.size}")
        if (closed.get() || frame.isEmpty()) {
            note("decode leave closed")
            return null
        }
        val codec = ensureDecoder(frame) ?: run {
            note("decode leave no decoder")
            return null
        }
        return try {
            val index = codec.dequeueInputBuffer(10_000)
            if (index < 0) {
                note("decode leave no input")
                return null
            }
            val input = codec.getInputBuffer(index) ?: run {
                note("decode leave empty input")
                return null
            }
            input.clear()
            input.put(frame)
            codec.queueInputBuffer(index, 0, frame.size, decoded * 1_000_000L / 8L, 0)
            decoded += 1
            val info = MediaCodec.BufferInfo()
            val out = codec.dequeueOutputBuffer(info, 10_000)
            if (out < 0) {
                note("decode leave ${outputName(out)}")
                return null
            }
            val image = codec.getOutputImage(out)
            val jpeg = if (image == null) {
                note("decode leave no image")
                null
            } else {
                val encoded = toJpeg(image)
                if (encoded == null) {
                    note(
                        "decode leave jpeg skipped ${image.width}x${image.height} planes ${image.planes.size}",
                    )
                } else {
                    note("decode leave jpeg ${encoded.size}")
                }
                encoded
            }
            image?.close()
            codec.releaseOutputBuffer(out, false)
            jpeg
        } catch (error: Exception) {
            Log.e(TAG, "A picture couldn't be shown", error)
            null
        }
    }

    private fun startEncoder(width: Int, height: Int): Boolean {
        return try {
            val codec = MediaCodec.createEncoderByType(MIME)
            val formats = codec.codecInfo.getCapabilitiesForType(MIME).colorFormats
            semiplanar = !formats.contains(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Planar) ||
                formats.contains(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420SemiPlanar)
            val color = if (semiplanar &&
                formats.contains(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420SemiPlanar)
            ) {
                MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420SemiPlanar
            } else if (formats.contains(MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Planar)) {
                semiplanar = false
                MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Planar
            } else {
                MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420SemiPlanar
            }
            val format = MediaFormat.createVideoFormat(MIME, width, height)
            format.setInteger(MediaFormat.KEY_COLOR_FORMAT, color)
            format.setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
            format.setInteger(MediaFormat.KEY_FRAME_RATE, FRAME_RATE)
            format.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
            format.setInteger(
                MediaFormat.KEY_BITRATE_MODE,
                MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR,
            )
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            codec.start()
            val input = codec.inputFormat
            pictureW = width
            pictureH = height
            stride = input.getInteger(MediaFormat.KEY_STRIDE, width).coerceAtLeast(width)
            slice = input.getInteger(MediaFormat.KEY_SLICE_HEIGHT, height).coerceAtLeast(height)
            encoder = codec
            note("encoder ${width}x${height} stride $stride slice $slice")
            true
        } catch (error: Exception) {
            Log.e(TAG, "The camera couldn't start", error)
            false
        }
    }

    private fun ensureDecoder(frame: ByteArray): MediaCodec? {
        val size = keyframeSize(frame)
        if (size != null && (decoder == null || size.first != decoderW || size.second != decoderH)) {
            releaseDecoder()
            return openDecoder(size.first, size.second)
        }
        return decoder ?: openDecoder(WIDTH, HEIGHT)
    }

    private fun openDecoder(width: Int, height: Int): MediaCodec? {
        return try {
            val codec = MediaCodec.createDecoderByType(MIME)
            val format = MediaFormat.createVideoFormat(MIME, width, height)
            codec.configure(format, null, null, 0)
            codec.start()
            decoder = codec
            decoderW = width
            decoderH = height
            note("decoder ${width}x${height}")
            codec
        } catch (error: Exception) {
            Log.e(TAG, "A picture couldn't be opened", error)
            null
        }
    }

    private fun releaseDecoder() {
        try {
            decoder?.stop()
        } catch (_: Exception) {
        }
        try {
            decoder?.release()
        } catch (_: Exception) {
        }
        decoder = null
    }

    private fun keyframeSize(frame: ByteArray): Pair<Int, Int>? {
        if (frame.size < 10 || frame[0].toInt() and 1 != 0) {
            return null
        }
        if (frame[3] != 0x9d.toByte() || frame[4] != 0x01.toByte() || frame[5] != 0x2a.toByte()) {
            return null
        }
        val width = (frame[6].toInt() and 0xff) or ((frame[7].toInt() and 0x3f) shl 8)
        val height = (frame[8].toInt() and 0xff) or ((frame[9].toInt() and 0x3f) shl 8)
        if (width < 2 || height < 2 || width > 640 || height > 480) {
            return null
        }
        return width to height
    }

    private fun onFrame(image: ImageProxy) {
        if (closed.get()) {
            image.close()
            return
        }
        val now = System.nanoTime()
        if (now - lastFrame < 120_000_000L) {
            image.close()
            return
        }
        lastFrame = now
        val i420 = i420(image)
        image.close()
        if (i420 == null) {
            note("camera frame skipped")
            return
        }
        offer(i420)
    }

    private fun offer(i420: ByteArray) {
        synchronized(codecs) {
            if (closed.get()) {
                return
            }
            offerLocked(i420)
        }
    }

    private fun offerLocked(i420: ByteArray) {
        if (encoder == null && !startEncoder(pictureW, pictureH)) {
            note("encoder didn't open")
            return
        }
        val codec = encoder ?: return
        var index = -1
        try {
            index = codec.dequeueInputBuffer(10_000)
            if (index < 0) {
                note("encoder no input")
                return
            }
            val buffer = codec.getInputBuffer(index)
            if (buffer == null) {
                note("encoder empty input")
                codec.queueInputBuffer(index, 0, 0, 0, 0)
                index = -1
                return
            }
            buffer.clear()
            if (semiplanar) {
                writeNv12(buffer, i420)
            } else {
                writePlanar(buffer, i420)
            }
            val pts = presented * 1_000_000L / 8L
            presented += 1
            codec.queueInputBuffer(index, 0, buffer.capacity(), pts, 0)
            index = -1
            drain(codec)
        } catch (error: Exception) {
            Log.e(TAG, "A frame couldn't be sent", error)
            if (index >= 0) {
                try {
                    codec.queueInputBuffer(index, 0, 0, 0, 0)
                } catch (_: Exception) {
                }
            }
        }
    }

    private fun drain(codec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (true) {
            val index = codec.dequeueOutputBuffer(info, 0)
            if (index < 0) {
                if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                    note("encoder size changed")
                }
                return
            }
            val out = codec.getOutputBuffer(index)
            if (out != null && info.size > 0) {
                val bytes = ByteArray(info.size)
                out.position(info.offset)
                out.get(bytes, 0, info.size)
                val kind = bufferKind(info.flags)
                if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                    note("sent ${bytes.size} $kind ${headOf(bytes)}")
                    nativeOffer(bytes)
                } else {
                    note("header ${bytes.size} $kind ${headOf(bytes)}")
                }
            }
            codec.releaseOutputBuffer(index, false)
        }
    }

    private fun writePlanar(buffer: ByteBuffer, i420: ByteArray) {
        val width = pictureW
        val height = pictureH
        writePlane(buffer, i420, 0, width, height, stride, 0)
        val chroma = stride / 2
        val uAt = stride * slice
        writePlane(buffer, i420, width * height, width / 2, height / 2, chroma, uAt)
        val vAt = uAt + chroma * (slice / 2)
        writePlane(buffer, i420, width * height + (width / 2) * (height / 2), width / 2, height / 2, chroma, vAt)
    }

    private fun writeNv12(buffer: ByteBuffer, i420: ByteArray) {
        val width = pictureW
        val height = pictureH
        writePlane(buffer, i420, 0, width, height, stride, 0)
        val uOff = width * height
        val vOff = uOff + (width / 2) * (height / 2)
        var row = 0
        while (row < height / 2) {
            val at = stride * slice + row * stride
            if (!fits(buffer, at, width)) {
                return
            }
            buffer.position(at)
            var col = 0
            while (col < width / 2) {
                val index = row * (width / 2) + col
                buffer.put(i420[uOff + index])
                buffer.put(i420[vOff + index])
                col += 1
            }
            row += 1
        }
    }

    private fun writePlane(
        buffer: ByteBuffer,
        src: ByteArray,
        srcOff: Int,
        width: Int,
        height: Int,
        rowStride: Int,
        dstOff: Int,
    ) {
        var row = 0
        while (row < height) {
            val at = dstOff + row * rowStride
            if (!fits(buffer, at, width)) {
                return
            }
            buffer.position(at)
            buffer.put(src, srcOff + row * width, width)
            row += 1
        }
    }

    private fun fits(buffer: ByteBuffer, at: Int, length: Int): Boolean {
        return at >= 0 && length > 0 && at <= buffer.capacity() - length
    }

    private fun toJpeg(image: Image): ByteArray? {
        val width = image.width
        val height = image.height
        if (width <= 0 || height <= 0 || width > 640 || height > 480 || width % 2 != 0 || height % 2 != 0) {
            return null
        }
        val nv21 = ByteArray(width * height * 3 / 2)
        val y = image.planes[0]
        var row = 0
        while (row < height) {
            var col = 0
            while (col < width) {
                val index = row * y.rowStride + col * y.pixelStride
                nv21[row * width + col] = if (index < y.buffer.limit()) y.buffer.get(index) else 0
                col += 1
            }
            row += 1
        }
        val u = image.planes.getOrNull(1) ?: return null
        val v = image.planes.getOrNull(2) ?: return null
        val chromaW = width / 2
        val chromaH = height / 2
        row = 0
        while (row < chromaH) {
            var col = 0
            while (col < chromaW) {
                val at = width * height + (row * chromaW + col) * 2
                val uIndex = row * u.rowStride + col * u.pixelStride
                val vIndex = row * v.rowStride + col * v.pixelStride
                nv21[at] = if (vIndex < v.buffer.limit()) v.buffer.get(vIndex) else 0
                nv21[at + 1] = if (uIndex < u.buffer.limit()) u.buffer.get(uIndex) else 0
                col += 1
            }
            row += 1
        }
        val jpeg = ByteArrayOutputStream()
        val yuv = YuvImage(nv21, ImageFormat.NV21, width, height, null)
        if (!yuv.compressToJpeg(Rect(0, 0, width, height), 55, jpeg)) {
            return null
        }
        return jpeg.toByteArray()
    }

    private fun i420(image: ImageProxy): ByteArray? {
        if (image.planes.size < 3) {
            return null
        }
        val rotation = image.imageInfo.rotationDegrees
        val bufferW = image.width
        val bufferH = image.height
        if (bufferW < 2 || bufferH < 2) {
            return null
        }
        val uprightW = if (rotation == 90 || rotation == 270) bufferH else bufferW
        val uprightH = if (rotation == 90 || rotation == 270) bufferW else bufferH
        val fitted = fitted(uprightW, uprightH)
        if (pictureW == 0 || pictureH == 0) {
            pictureW = fitted.first
            pictureH = fitted.second
        }
        val outW = pictureW
        val outH = pictureH
        val out = ByteArray(outW * outH * 3 / 2)
        val y = image.planes[0]
        val u = image.planes[1]
        val v = image.planes[2]
        var row = 0
        while (row < outH) {
            var col = 0
            while (col < outW) {
                val ux = col * uprightW / outW
                val uy = row * uprightH / outH
                val (bx, by) = bufferPoint(ux, uy, bufferW, bufferH, rotation)
                val yIndex = by * y.rowStride + bx * y.pixelStride
                out[row * outW + col] = if (yIndex < y.buffer.limit()) y.buffer.get(yIndex) else 0
                if (row % 2 == 0 && col % 2 == 0) {
                    val cx = bx / 2
                    val cy = by / 2
                    val uIndex = cy * u.rowStride + cx * u.pixelStride
                    val vIndex = cy * v.rowStride + cx * v.pixelStride
                    val chroma = (row / 2) * (outW / 2) + col / 2
                    out[outW * outH + chroma] =
                        if (uIndex < u.buffer.limit()) u.buffer.get(uIndex) else 0
                    out[outW * outH + (outW / 2) * (outH / 2) + chroma] =
                        if (vIndex < v.buffer.limit()) v.buffer.get(vIndex) else 0
                }
                col += 1
            }
            row += 1
        }
        return out
    }

    private fun releaseCodecs() {
        try {
            encoder?.stop()
        } catch (_: Exception) {
        }
        try {
            encoder?.release()
        } catch (_: Exception) {
        }
        encoder = null
        releaseDecoder()
    }

    private fun note(message: String) {
        Log.i(TAG, "tid ${android.os.Process.myTid()} $message")
    }

    private fun bufferKind(flags: Int): String {
        val names = ArrayList<String>()
        if (flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0) {
            names.add("key")
        }
        if (flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
            names.add("header")
        }
        if (flags and MediaCodec.BUFFER_FLAG_PARTIAL_FRAME != 0) {
            names.add("partial")
        }
        if (flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) {
            names.add("end")
        }
        if (names.isEmpty()) {
            return "delta"
        }
        return names.joinToString("+")
    }

    private fun headOf(bytes: ByteArray): String {
        val count = minOf(4, bytes.size)
        val text = StringBuilder()
        var index = 0
        while (index < count) {
            text.append("%02x".format(bytes[index]))
            index += 1
        }
        return text.toString()
    }

    private fun outputName(code: Int): String {
        return when (code) {
            MediaCodec.INFO_TRY_AGAIN_LATER -> "not ready"
            MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> "size changed"
            MediaCodec.INFO_OUTPUT_BUFFERS_CHANGED -> "buffers changed"
            else -> "code $code"
        }
    }

    private external fun nativeOffer(frame: ByteArray)

    private companion object {
        private const val TAG = "Journal"
        private const val MIME = "video/x-vnd.on2.vp8"
        private const val LONG = 160
        private const val WIDTH = 160
        private const val HEIGHT = 120
        private const val BITRATE = 160_000
        private const val FRAME_RATE = 8

        private fun fitted(width: Int, height: Int): Pair<Int, Int> {
            if (width < 2 || height < 2) {
                return WIDTH to HEIGHT
            }
            val wide = width >= height
            val longSide = width.coerceAtLeast(height)
            val shortSide = width.coerceAtMost(height)
            var across = (LONG.toLong() * shortSide / longSide).toInt()
            across = across and 1.inv()
            if (across < 2) {
                across = 2
            }
            return if (wide) LONG to across else across to LONG
        }

        private fun frameSize(): ResolutionSelector {
            return ResolutionSelector.Builder()
                .setResolutionStrategy(
                    ResolutionStrategy(
                        Size(320, 240),
                        ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER,
                    ),
                )
                .build()
        }

        private fun bufferPoint(ux: Int, uy: Int, width: Int, height: Int, rotation: Int): Pair<Int, Int> {
            return when (rotation) {
                90 -> uy to (height - 1 - ux).coerceAtLeast(0)
                180 -> (width - 1 - ux).coerceAtLeast(0) to (height - 1 - uy).coerceAtLeast(0)
                270 -> (width - 1 - uy).coerceAtLeast(0) to ux
                else -> ux to uy
            }
        }
    }
}
