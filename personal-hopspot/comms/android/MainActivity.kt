package dev.dioxus.main

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.util.Log
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicReference
import org.personal.prns.controller.BleLink
import org.personal.prns.controller.UsbLink

typealias BuildConfig = org.personal.prns.comms.BuildConfig

class MainActivity : WryActivity() {
    private var waiting: CountDownLatch? = null
    private var found: AtomicReference<String?>? = null
    private var micWaiting: CountDownLatch? = null
    private var micStatus: Int = 1
    private var wifi: WifiLan? = null
    private var scanner: CodeScan? = null
    private var ble: BleLink? = null
    private var usb: UsbLink? = null
    private var earLock: PowerManager.WakeLock? = null

    private val permission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                openScanner()
            } else {
                finishScan(null, DENIED)
            }
        }

    private val microphone =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            micStatus = if (granted) 0 else 1
            micWaiting?.countDown()
            micWaiting = null
        }

    private val radios =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
            startRadios()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        bindToWifi()
        super.onCreate(savedInstanceState)
        instance = this
        val lan = WifiLan(this)
        lan.start()
        wifi = lan
        startRadios()
        requestRadioPermissions()
    }

    override fun onResume() {
        super.onResume()
        startRadios()
    }

    private fun startRadios() {
        if (usb == null) {
            usb = runCatching { UsbLink(applicationContext).also { it.start() } }
                .onFailure { Log.e("Journal", "USB didn't open", it) }
                .getOrNull()
        }
        if (ble == null && hasBlePermissions()) {
            ble = runCatching { BleLink(applicationContext).also { it.start() } }
                .onFailure { Log.e("Journal", "BLE didn't open", it) }
                .getOrNull()
        }
    }

    private fun requestRadioPermissions() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
            return
        }
        val needed = listOf(
            Manifest.permission.BLUETOOTH_SCAN,
            Manifest.permission.BLUETOOTH_ADVERTISE,
            Manifest.permission.BLUETOOTH_CONNECT,
        ).filter { permission ->
            ContextCompat.checkSelfPermission(this, permission) != PackageManager.PERMISSION_GRANTED
        }
        if (needed.isNotEmpty()) {
            radios.launch(needed.toTypedArray())
        }
    }

    private fun hasBlePermissions(): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
            return false
        }
        return listOf(
            Manifest.permission.BLUETOOTH_SCAN,
            Manifest.permission.BLUETOOTH_ADVERTISE,
            Manifest.permission.BLUETOOTH_CONNECT,
        ).all { permission ->
            ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED
        }
    }

    private fun bindToWifi() {
        val connectivity = getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
            ?: return
        val wifi = connectivity.allNetworks.firstOrNull { network ->
            connectivity.getNetworkCapabilities(network)
                ?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
        } ?: return
        if (connectivity.bindProcessToNetwork(wifi)) {
            Log.i("JournalWifi", "bound process to wifi")
        }
    }

    override fun onDestroy() {
        scanner?.cancel()
        scanner = null
        runCatching { ble?.stop() }
        runCatching { usb?.stop() }
        ble = null
        usb = null
        wifi?.stop()
        wifi = null
        if (instance === this) {
            instance = null
        }
        finishScan(null, CANCELLED)
        micWaiting?.countDown()
        micWaiting = null
        super.onDestroy()
    }

    fun machineName(): String = Build.MODEL ?: ""

    fun beginCall() {
        val latch = CountDownLatch(1)
        runOnUiThread {
            try {
                val audio = getSystemService(AUDIO_SERVICE) as AudioManager
                audio.mode = AudioManager.MODE_IN_COMMUNICATION
                @Suppress("DEPRECATION")
                audio.isSpeakerphoneOn = false
                if (earLock?.isHeld != true) {
                    val power = getSystemService(POWER_SERVICE) as PowerManager
                    @Suppress("DEPRECATION")
                    val lock = power.newWakeLock(
                        PowerManager.PROXIMITY_SCREEN_OFF_WAKE_LOCK,
                        "journal:call",
                    )
                    lock.setReferenceCounted(false)
                    lock.acquire()
                    earLock = lock
                }
            } finally {
                latch.countDown()
            }
        }
        latch.await()
    }

    fun endCall() {
        val latch = CountDownLatch(1)
        runOnUiThread {
            try {
                earLock?.let { lock ->
                    if (lock.isHeld) {
                        lock.release()
                    }
                }
                earLock = null
                val audio = getSystemService(AUDIO_SERVICE) as AudioManager
                audio.mode = AudioManager.MODE_NORMAL
            } finally {
                latch.countDown()
            }
        }
        latch.await()
    }

    fun allowMicrophone(): Int {
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            == PackageManager.PERMISSION_GRANTED
        ) {
            return 0
        }
        val latch = CountDownLatch(1)
        runOnUiThread {
            micWaiting = latch
            microphone.launch(Manifest.permission.RECORD_AUDIO)
        }
        latch.await()
        return micStatus
    }

    fun beginScan(latch: CountDownLatch, output: AtomicReference<String?>) {
        waiting = latch
        found = output
        val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            openScanner()
        } else {
            permission.launch(Manifest.permission.CAMERA)
        }
    }

    private fun openScanner() {
        val scan = CodeScan(this)
        scanner = scan
        scan.start(
            onCode = { text ->
                scanner = null
                finishScan(text, CANCELLED)
            },
            onUnavailable = {
                scanner = null
                finishScan(null, UNAVAILABLE)
            },
        )
    }

    fun finishScan(text: String?, outcome: Int) {
        val latch = waiting ?: return
        if (text != null) {
            status = CODE
            found?.set(text)
        } else {
            status = outcome
            found?.set(null)
        }
        waiting = null
        found = null
        scanner = null
        latch.countDown()
    }

    companion object {
        const val CANCELLED = 0
        const val DENIED = 1
        const val UNAVAILABLE = 2
        const val CODE = 3

        @JvmField
        var status: Int = UNAVAILABLE

        @Volatile
        private var instance: MainActivity? = null

        @JvmStatic
        fun capture(): String? {
            val activity = instance ?: run {
                status = UNAVAILABLE
                return null
            }
            val latch = CountDownLatch(1)
            val output = AtomicReference<String?>()
            activity.runOnUiThread { activity.beginScan(latch, output) }
            latch.await()
            return output.get()
        }
    }
}
