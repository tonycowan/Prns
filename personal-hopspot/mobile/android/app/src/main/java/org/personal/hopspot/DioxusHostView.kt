package org.personal.hopspot

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.ConsoleMessage
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.WebViewAssetLoader
import java.io.IOException
import org.json.JSONObject
import org.json.JSONTokener

/**
 * Hosts the Dioxus web build from assets and bridges live engine calls to
 * [PrnsService] via [HopspotJsBridge].
 *
 * Uses [WebViewAssetLoader] so WASM/module scripts load over
 * `https://appassets.androidplatform.net` — `file://` cannot fetch WASM.
 */
@SuppressLint("SetJavaScriptEnabled")
class DioxusHostView(
    context: Context,
) : WebView(context) {
    private val mainHandler = Handler(Looper.getMainLooper())
    private val bridge = HopspotJsBridge()
    private val assetLoader =
        WebViewAssetLoader.Builder()
            .addPathHandler("/assets/", MimeAwareAssetsHandler(context.applicationContext))
            .build()

    init {
        setBackgroundColor(0xFF0B0D10.toInt())
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        settings.allowFileAccess = false
        settings.cacheMode = WebSettings.LOAD_NO_CACHE
        clearCache(true)
        clearFormData()
        clearHistory()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.KITKAT) {
            setWebContentsDebuggingEnabled(true)
        }
        webChromeClient =
            object : WebChromeClient() {
                override fun onConsoleMessage(consoleMessage: ConsoleMessage): Boolean {
                    val message = consoleMessage.message()
                    // Copy must not go through @JavascriptInterface during a click —
                    // that path froze the WASM UI on RNS Config. UI logs this marker;
                    // we then copy from the native snapshot (real newlines).
                    if (message.contains(COPY_READY_MARKER)) {
                        pullPendingCopyToClipboard()
                    }
                    Log.i(
                        TAG,
                        "${consoleMessage.messageLevel()} ${consoleMessage.sourceId()}:${consoleMessage.lineNumber()} $message",
                    )
                    return true
                }
            }
        webViewClient =
            object : WebViewClient() {
                override fun shouldInterceptRequest(
                    view: WebView,
                    request: WebResourceRequest,
                ): WebResourceResponse? = assetLoader.shouldInterceptRequest(request.url)
            }
        addJavascriptInterface(bridge, "HopspotBridge")
        // Served from APK assets/ via WebViewAssetLoader (not file://).
        loadUrl("$ASSET_BASE/dioxus/index.html")
    }

    fun setService(service: PrnsService?) {
        bridge.service = service
    }

    /**
     * Copy the live Sideband join config to the clipboard.
     *
     * Prefer the native snapshot JSON (proper newlines) over `evaluateJavascript`,
     * which can leave literal `\n` sequences if decoding fails.
     */
    private fun pullPendingCopyToClipboard() {
        mainHandler.postDelayed({
            val nativeText = rnsConfigFromSnapshot(bridge.service?.uiSnapshotJson())
            if (!nativeText.isNullOrEmpty()) {
                setClipboard(nativeText, "native snapshot")
                return@postDelayed
            }
            evaluateJavascript(PULL_PENDING_COPY_JS) { raw ->
                val text = jsStringResult(raw)
                if (text.isNullOrEmpty()) {
                    Log.w(TAG, "pending copy empty (raw=$raw)")
                    return@evaluateJavascript
                }
                setClipboard(text, "js fallback")
            }
        }, 200)
    }

    private fun setClipboard(text: String, source: String) {
        val clipboard =
            context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
        if (clipboard == null) {
            Log.w(TAG, "no ClipboardManager")
            return
        }
        clipboard.setPrimaryClip(ClipData.newPlainText("RNS config", text))
        Log.i(TAG, "copied ${text.length} chars via $source")
    }

    class HopspotJsBridge {
        @Volatile
        var service: PrnsService? = null

        @JavascriptInterface
        fun getSnapshot(): String =
            service?.uiSnapshotJson()
                ?: """{"engine":"stopped","uptime_ms":0,"interface_count":0,"online_interface_count":0,"rx_bytes":0,"tx_bytes":0,"local_rns_port":37428,"rpc_port":37429,"rpc_key_hex":null,"cards":[],"limits":[],"rns_config":""}"""

        @JavascriptInterface
        fun announce() {
            service?.announce()
        }

        @JavascriptInterface
        fun sleepInterfaces() {
            service?.sleepInterfaces()
        }

        @JavascriptInterface
        fun wakeInterfaces() {
            service?.wakeInterfaces()
        }

        @JavascriptInterface
        fun toggleInterface(idHex: String) {
            service?.toggleInterface(idHex)
        }

        @JavascriptInterface
        fun setBleDiscoveryGroup(groupId: String): Boolean =
            service?.setBleDiscoveryGroup(groupId) ?: false
    }

    /** Serves APK assets with MIME types WebView needs for ES modules + WASM. */
    private class MimeAwareAssetsHandler(
        context: Context,
    ) : WebViewAssetLoader.PathHandler {
        private val assets = context.assets

        override fun handle(path: String): WebResourceResponse? {
            val assetPath = path.trimStart('/')
            val candidates =
                listOf(
                    assetPath,
                    "dioxus/$assetPath",
                    "dioxus/assets/$assetPath",
                    assetPath.removePrefix("dioxus/"),
                ).distinct()
            for (candidate in candidates) {
                val mime = mimeFor(candidate)
                try {
                    return WebResourceResponse(
                        mime,
                        Charsets.UTF_8.name(),
                        assets.open(candidate),
                    )
                } catch (_: IOException) {
                    // try next
                }
            }
            Log.w(TAG, "asset miss: $assetPath")
            return null
        }

        private fun mimeFor(assetPath: String): String =
            when {
                assetPath.endsWith(".wasm") -> "application/wasm"
                assetPath.endsWith(".js") || assetPath.endsWith(".mjs") -> "text/javascript"
                assetPath.endsWith(".css") -> "text/css"
                assetPath.endsWith(".html") || assetPath.endsWith(".htm") -> "text/html"
                assetPath.endsWith(".json") -> "application/json"
                assetPath.endsWith(".svg") -> "image/svg+xml"
                assetPath.endsWith(".png") -> "image/png"
                assetPath.endsWith(".woff2") -> "font/woff2"
                else -> "application/octet-stream"
            }
    }

    private companion object {
        private const val TAG = "HopspotDioxus"
        private const val ASSET_BASE = "https://appassets.androidplatform.net/assets"
        private const val COPY_READY_MARKER = "HOPSPOT_COPY_READY"
        private const val PULL_PENDING_COPY_JS =
            "(function(){var t=window.__hopspotPendingCopy; window.__hopspotPendingCopy=null; return (t==null)?'':String(t);})()"

        private fun rnsConfigFromSnapshot(snapshotJson: String?): String? {
            if (snapshotJson.isNullOrBlank()) {
                return null
            }
            return try {
                JSONObject(snapshotJson).optString("rns_config").takeIf { it.isNotEmpty() }
            } catch (_: Exception) {
                null
            }
        }

        private fun jsStringResult(raw: String?): String? {
            if (raw.isNullOrBlank() || raw == "null") {
                return null
            }
            val parsed =
                try {
                    JSONTokener(raw).nextValue() as? String
                } catch (_: Exception) {
                    null
                } ?: return null
            // Some WebView builds leave write escapes intact if JSON decode is skipped.
            return if (parsed.contains('\\') && parsed.contains("\\n") && !parsed.contains('\n')) {
                parsed.replace("\\n", "\n").replace("\\t", "\t")
            } else {
                parsed
            }
        }
    }
}
