package org.personal.hopspot

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
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
    private val bridge = HopspotJsBridge(context.applicationContext)
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
        // Served from APK assets/ via WebViewAssetLoader (not file://).
        loadUrl("$ASSET_BASE/dioxus/index.html")
    }

    fun setService(service: PrnsService?) {
        bridge.service = service
    }

    class HopspotJsBridge(
        private val appContext: Context,
    ) {
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
        fun copyText(text: String) {
            val app = appContext
            android.os.Handler(android.os.Looper.getMainLooper()).post {
                val clipboard =
                    app.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                        ?: return@post
                clipboard.setPrimaryClip(ClipData.newPlainText("RNS config", text))
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
    }
}
