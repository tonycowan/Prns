package org.personal.hopspot

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.webkit.JavascriptInterface
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * Hosts the Dioxus web build from assets and bridges live engine calls to
 * [PrnsService] via [HopspotJsBridge].
 */
@SuppressLint("SetJavaScriptEnabled")
class DioxusHostView(
    context: Context,
) : WebView(context) {
    private val bridge = HopspotJsBridge(context.applicationContext)

    init {
        setBackgroundColor(0xFF0B0D10.toInt())
        settings.javaScriptEnabled = true
        settings.domStorageEnabled = true
        settings.allowFileAccess = true
        settings.cacheMode = WebSettings.LOAD_NO_CACHE
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.JELLY_BEAN) {
            @Suppress("DEPRECATION")
            settings.allowFileAccessFromFileURLs = true
            @Suppress("DEPRECATION")
            settings.allowUniversalAccessFromFileURLs = true
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.KITKAT) {
            setWebContentsDebuggingEnabled(true)
        }
        webViewClient = WebViewClient()
        addJavascriptInterface(bridge, "HopspotBridge")
        loadUrl("file:///android_asset/dioxus/index.html")
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
            val clipboard =
                appContext.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                    ?: return
            clipboard.setPrimaryClip(ClipData.newPlainText("RNS config", text))
        }
    }
}
