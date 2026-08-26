package org.personal.hopspot

import android.Manifest
import android.app.Activity
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder

/**
 * Launcher Activity. Default (`dioxus` flavor) hosts the Dioxus management UI;
 * `oled` flavor keeps the pixel face for regression.
 *
 * Both flavors only bind [PrnsService] — closing this Activity does not stop the
 * engine.
 */
class MainActivity : Activity() {
    private var service: PrnsService? = null
    private var bound = false
    private var hopspotView: HopspotView? = null
    private var dioxusView: DioxusHostView? = null
    private var refreshLinksOnConnect = false
    private var lastPermissionState: List<Boolean>? = null

    private val serviceConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val local = binder as? PrnsService.LocalBinder ?: return
            service = local.service
            hopspotView?.setService(local.service)
            dioxusView?.setService(local.service)
            if (refreshLinksOnConnect) {
                refreshLinksOnConnect = false
                local.service.refreshPlatformLinks()
            }
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
            hopspotView?.setService(null)
            dioxusView?.setService(null)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (BuildConfig.UI_FACE == "oled") {
            hopspotView = HopspotView(this).also { setContentView(it) }
        } else {
            dioxusView = DioxusHostView(this).also { setContentView(it) }
        }
        startAndBindService()
        requestMissingPermissions()
    }

    override fun onDestroy() {
        super.onDestroy()
        hopspotView?.stop()
        hopspotView?.setService(null)
        hopspotView = null
        dioxusView?.setService(null)
        dioxusView = null
        if (bound) {
            unbindService(serviceConnection)
            bound = false
        }
        service = null
    }

    override fun onResume() {
        super.onResume()
        val current = permissionState()
        val changed = lastPermissionState?.let { it != current } == true
        lastPermissionState = current
        if (changed) {
            refreshPlatformLinks()
        }
    }

    private fun requestMissingPermissions() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
            return
        }
        val needed = runtimePermissions().filter { permission ->
            checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED
        }
        if (needed.isNotEmpty()) {
            requestPermissions(needed.toTypedArray(), PRNS_PERMISSION_REQUEST)
        }
    }

    private fun runtimePermissions(): List<String> {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
            return emptyList()
        }
        val permissions = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            permissions += Manifest.permission.POST_NOTIFICATIONS
            permissions += Manifest.permission.NEARBY_WIFI_DEVICES
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            permissions += listOf(
                Manifest.permission.BLUETOOTH_SCAN,
                Manifest.permission.BLUETOOTH_ADVERTISE,
                Manifest.permission.BLUETOOTH_CONNECT,
            )
        }
        if (Build.VERSION.SDK_INT <= Build.VERSION_CODES.S_V2) {
            permissions += listOf(
                Manifest.permission.ACCESS_COARSE_LOCATION,
                Manifest.permission.ACCESS_FINE_LOCATION,
            )
        }
        return permissions
    }

    private fun startAndBindService() {
        PrnsService.start(this)
        if (!bound) {
            bound = bindService(
                Intent(this, PrnsService::class.java),
                serviceConnection,
                Context.BIND_AUTO_CREATE,
            )
        } else {
            service?.refreshPlatformLinks()
        }
    }

    private fun permissionState(): List<Boolean> {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) {
            return emptyList()
        }
        return runtimePermissions().map {
            checkSelfPermission(it) == PackageManager.PERMISSION_GRANTED
        }
    }

    private fun refreshPlatformLinks() {
        val current = service
        if (current == null) {
            refreshLinksOnConnect = true
        } else {
            current.refreshPlatformLinks()
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != PRNS_PERMISSION_REQUEST) {
            return
        }
        lastPermissionState = permissionState()
        refreshPlatformLinks()
    }

    private companion object {
        private const val PRNS_PERMISSION_REQUEST = 1
    }
}
