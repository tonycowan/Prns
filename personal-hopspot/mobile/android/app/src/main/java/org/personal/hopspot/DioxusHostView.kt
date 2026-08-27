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

/**
 * Hosts the Dioxus web build from assets and bridges live engine calls to
 * [PrnsService] via [HopspotJsBridge].
 *
 * Uses [WebViewAssetLoader] so WASM/module scripts load over
 * `https://appassets.androidplatform.net` — `file://` cannot fetch WASM.
 *
 * Important: `@JavascriptInterface` calls block the WebView UI thread until they
 * return. Mutation methods must only `post` work and return immediately — doing
 * engine/clipboard work inline deadlocks the UI (buttons stop responding).
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
                    Log.i(
                        TAG,
                        "${consoleMessage.messageLevel()} ${consoleMessage.sourceId()}:${consoleMessage.lineNumber()} ${consoleMessage.message()}",
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
        loadUrl("$ASSET_BASE/dioxus/index.html")
    }

    fun setService(service: PrnsService?) {
        bridge.service = service
    }

    private fun setClipboard(text: String) {
        val clipboard =
            context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
        if (clipboard == null) {
            Log.w(TAG, "no ClipboardManager")
            return
        }
        clipboard.setPrimaryClip(ClipData.newPlainText("RNS config", text))
    }

    inner class HopspotJsBridge {
        @Volatile
        var service: PrnsService? = null

        @JavascriptInterface
        fun getSnapshot(): String =
            service?.uiSnapshotJson()
                ?: """{"engine":"stopped","uptime_ms":0,"interface_count":0,"online_interface_count":0,"rx_bytes":0,"tx_bytes":0,"local_rns_port":37428,"rpc_port":37429,"rpc_key_hex":null,"cards":[],"limits":[],"rns_config":""}"""

        @JavascriptInterface
        fun announce() {
            mainHandler.post { service?.announce() }
        }

        @JavascriptInterface
        fun sleepInterfaces() {
            mainHandler.post { service?.sleepInterfaces() }
        }

        @JavascriptInterface
        fun wakeInterfaces() {
            mainHandler.post { service?.wakeInterfaces() }
        }

        @JavascriptInterface
        fun toggleInterface(idHex: String) {
            mainHandler.post { service?.toggleInterface(idHex) }
        }

        @JavascriptInterface
        fun copyRnsConfig() {
            mainHandler.post {
                val text = rnsConfigFromSnapshot(service?.uiSnapshotJson())
                if (text.isNullOrEmpty()) {
                    Log.w(TAG, "copyRnsConfig: empty config")
                    return@post
                }
                setClipboard(text)
            }
        }
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
    }
}
