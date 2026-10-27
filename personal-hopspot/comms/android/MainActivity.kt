package dev.dioxus.main

import android.Manifest
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ContentUris
import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.content.Context
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.location.Location
import android.location.LocationListener
import android.location.LocationManager
import android.media.AudioManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.os.Build
import android.os.Environment
import android.os.Bundle
import android.os.Looper
import android.os.PowerManager
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.provider.Settings
import android.webkit.MimeTypeMap
import android.util.Log
import android.view.View
import android.webkit.WebView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference
import org.personal.prns.controller.BleLink
import org.personal.prns.controller.UsbLink

typealias BuildConfig = org.personal.prns.comms.BuildConfig

class MainActivity : WryActivity() {
    private var waiting: CountDownLatch? = null
    private var found: AtomicReference<String?>? = null
    private var micWaiting: CountDownLatch? = null
    private var micStatus: Int = 1
    private var placeWaiting: CountDownLatch? = null
    private var placeGranted: Boolean = false
    private var fileLatch: CountDownLatch? = null
    private var fileResult: AtomicReference<String?>? = null
    private var fileLimit: Long = 0
    private var wifi: WifiLan? = null
    private var scanner: CodeScan? = null
    private var film: Film? = null
    private var filmLatch: CountDownLatch? = null
    private var filmResult: AtomicInteger? = null
    private var ble: BleLink? = null
    private var usb: UsbLink? = null
    private var earLock: PowerManager.WakeLock? = null

    @Volatile
    private var open: Boolean = false

    private val permission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                openScanner()
            } else {
                finishScan(null, DENIED)
            }
        }

    private val filmPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            val result = filmResult
            val latch = filmLatch
            filmLatch = null
            filmResult = null
            if (granted && result != null && latch != null) {
                startFilm(result, latch)
            } else {
                latch?.countDown()
            }
        }

    private val microphone =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            micStatus = if (granted) 0 else 1
            micWaiting?.countDown()
            micWaiting = null
        }

    private val placePermission =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { granted ->
            placeGranted = granted[Manifest.permission.ACCESS_FINE_LOCATION] == true
                || granted[Manifest.permission.ACCESS_COARSE_LOCATION] == true
            placeWaiting?.countDown()
            placeWaiting = null
        }

    private val openFile =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
            val saved = fileResult
            val latch = fileLatch
            val limit = fileLimit
            fileResult = null
            fileLatch = null
            if (latch == null || saved == null) {
                return@registerForActivityResult
            }
            val uri = if (result.resultCode == Activity.RESULT_OK) result.data?.data else null
            if (uri == null) {
                latch.countDown()
                return@registerForActivityResult
            }
            kotlin.concurrent.thread(name = "note-file") {
                saved.set(copyChosen(uri, limit))
                latch.countDown()
            }
        }

    private val radios =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
            startRadios()
        }

    private val notesPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            askBattery()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        process = applicationContext
        bindToWifi()
        super.onCreate(savedInstanceState)
        instance = this
        val lan = WifiLan(this)
        lan.start()
        wifi = lan
        startRadios()
        requestRadioPermissions()
        askNotes()
    }

    override fun onWebViewCreate(webView: WebView) {
        webView.overScrollMode = View.OVER_SCROLL_NEVER
    }

    override fun onResume() {
        super.onResume()
        open = true
        startRadios()
    }

    override fun onPause() {
        open = false
        super.onPause()
    }

    fun showing(): Boolean = open

    fun showNote(name: String, words: String) {
        postNote(name, words)
    }

    private fun askNotes() {
        if (Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            notesPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            askBattery()
        }
    }

    private fun askBattery() {
        if (askedPower) {
            return
        }
        askedPower = true
        val power = getSystemService(PowerManager::class.java) ?: return
        if (power.isIgnoringBatteryOptimizations(packageName)) {
            return
        }
        val intent = Intent(
            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:$packageName"),
        )
        runCatching { startActivity(intent) }
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

    fun beginVideoCall() {
        val latch = CountDownLatch(1)
        runOnUiThread {
            try {
                val audio = getSystemService(AUDIO_SERVICE) as AudioManager
                audio.mode = AudioManager.MODE_IN_COMMUNICATION
                @Suppress("DEPRECATION")
                audio.isSpeakerphoneOn = true
            } finally {
                latch.countDown()
            }
        }
        latch.await()
    }

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
                @Suppress("DEPRECATION")
                audio.isSpeakerphoneOn = false
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

    fun readPlace(): String? {
        if (!ensureLocationPermission()) {
            return null
        }
        val manager = getSystemService(LocationManager::class.java) ?: return null
        val provider = chooseProvider(manager) ?: return null
        recent(manager, provider)?.let { return it }
        return awaitFix(manager, provider)
    }

    private fun chooseProvider(manager: LocationManager): String? {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
            manager.isProviderEnabled(LocationManager.FUSED_PROVIDER)
        ) {
            return LocationManager.FUSED_PROVIDER
        }
        if (manager.isProviderEnabled(LocationManager.GPS_PROVIDER)) {
            return LocationManager.GPS_PROVIDER
        }
        if (manager.isProviderEnabled(LocationManager.NETWORK_PROVIDER)) {
            return LocationManager.NETWORK_PROVIDER
        }
        return null
    }

    private fun recent(manager: LocationManager, provider: String): String? {
        val location = try {
            manager.getLastKnownLocation(provider)
        } catch (_: SecurityException) {
            null
        } ?: return null
        if (System.currentTimeMillis() - location.time > 120_000) {
            return null
        }
        return format(location)
    }

    private fun awaitFix(manager: LocationManager, provider: String): String? {
        val latch = CountDownLatch(1)
        val found = AtomicReference<Location?>(null)
        val listener = LocationListener { location ->
            found.set(location)
            latch.countDown()
        }
        try {
            manager.requestLocationUpdates(provider, 0L, 0f, listener, Looper.getMainLooper())
        } catch (_: SecurityException) {
            return null
        } catch (_: IllegalArgumentException) {
            return null
        }
        try {
            if (!latch.await(45, TimeUnit.SECONDS)) {
                return null
            }
        } finally {
            manager.removeUpdates(listener)
        }
        return found.get()?.let { format(it) }
    }

    private fun format(location: Location) = "${location.latitude},${location.longitude}"

    fun pickFile(maxBytes: Long): String? {
        val latch = CountDownLatch(1)
        val result = AtomicReference<String?>(null)
        runOnUiThread {
            fileLatch = latch
            fileResult = result
            fileLimit = maxBytes
            try {
                // A MIME list of "*/*" makes the document picker keep a narrow type filter.
                val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                    addCategory(Intent.CATEGORY_OPENABLE)
                    type = "*/*"
                }
                openFile.launch(intent)
            } catch (_: Exception) {
                latch.countDown()
            }
        }
        if (!latch.await(10, TimeUnit.MINUTES)) {
            return null
        }
        return result.get()
    }

    private fun copyChosen(uri: Uri, maxBytes: Long): String? {
        val name = displayName(uri)
        val dest = File(cacheDir, "note-${System.nanoTime()}")
        var total = 0L
        var tooBig = false
        try {
            val input = contentResolver.openInputStream(uri) ?: return "missing"
            input.use { stream ->
                dest.outputStream().use { output ->
                    val buf = ByteArray(8192)
                    while (true) {
                        val n = stream.read(buf)
                        if (n < 0) {
                            break
                        }
                        total += n.toLong()
                        if (maxBytes > 0L && total > maxBytes) {
                            tooBig = true
                            break
                        }
                        output.write(buf, 0, n)
                    }
                }
            }
        } catch (_: Exception) {
            dest.delete()
            return "missing"
        }
        if (tooBig || total == 0L) {
            dest.delete()
            return if (tooBig) "over" else "missing"
        }
        return "$name\n${dest.absolutePath}"
    }

    private fun displayName(uri: Uri): String {
        val cursor = contentResolver.query(
            uri,
            arrayOf(OpenableColumns.DISPLAY_NAME),
            null,
            null,
            null,
        )
        val raw = cursor?.use { rows ->
            if (!rows.moveToFirst()) {
                null
            } else {
                val index = rows.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (index < 0) null else rows.getString(index)
            }
        }
        val name = raw
            ?.replace('\n', ' ')
            ?.replace('\r', ' ')
            ?.trim()
            ?.substringAfterLast('/')
            ?.substringAfterLast('\\')
            ?.take(120)
            ?.trim()
            .orEmpty()
        return name.ifBlank { "file" }
    }

    fun writeDownload(sourcePath: String, displayName: String): String? {
        val source = File(sourcePath)
        if (!source.isFile || source.length() == 0L) {
            return null
        }
        val safe = displayName.replace('\n', ' ').replace('\r', ' ').trim().ifBlank { "file" }
        for (n in 1 until 1000) {
            val candidate = if (n == 1) safe else numberedDownload(safe, n)
            val found = findDownload(candidate)
            if (found == null) {
                return if (insertDownload(source, candidate)) candidate else null
            }
            if (found.second == source.length()) {
                return candidate
            }
        }
        return null
    }

    fun viewDownload(displayName: String) {
        val found = findDownload(displayName) ?: return
        val mime = MimeTypeMap.getSingleton()
            .getMimeTypeFromExtension(File(displayName).extension.lowercase())
            ?: "application/octet-stream"
        val view = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(found.first, mime)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
        runCatching { startActivity(Intent.createChooser(view, displayName)) }
    }

    private fun findDownload(name: String): Pair<Uri, Long>? {
        val collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI
        val cursor = runCatching {
            contentResolver.query(
                collection,
                arrayOf(MediaStore.MediaColumns._ID, MediaStore.MediaColumns.SIZE),
                "${MediaStore.MediaColumns.DISPLAY_NAME}=?",
                arrayOf(name),
                null,
            )
        }.getOrNull() ?: return null
        cursor.use { rows ->
            if (!rows.moveToFirst()) {
                return null
            }
            val id = rows.getLong(0)
            val size = rows.getLong(1)
            return ContentUris.withAppendedId(collection, id) to size
        }
    }

    private fun insertDownload(source: File, name: String): Boolean {
        val mime = MimeTypeMap.getSingleton()
            .getMimeTypeFromExtension(File(name).extension.lowercase())
            ?: "application/octet-stream"
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, mime)
            put(MediaStore.MediaColumns.SIZE, source.length())
            put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS)
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val uri = contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
            ?: return false
        val wrote = runCatching {
            val output = contentResolver.openOutputStream(uri) ?: return@runCatching false
            output.use { out ->
                source.inputStream().use { input -> input.copyTo(out) }
            }
            true
        }.getOrDefault(false)
        if (!wrote) {
            contentResolver.delete(uri, null, null)
            return false
        }
        values.clear()
        values.put(MediaStore.MediaColumns.IS_PENDING, 0)
        contentResolver.update(uri, values, null, null)
        return true
    }

    private fun numberedDownload(name: String, n: Int): String {
        val dot = name.lastIndexOf('.')
        if (dot <= 0 || dot == name.lastIndex) {
            return "$name ($n)"
        }
        return "${name.substring(0, dot)} ($n)${name.substring(dot)}"
    }

    fun openPlace(query: String) {
        val intent = Intent(Intent.ACTION_VIEW, Uri.parse(query))
        runOnUiThread {
            try {
                startActivity(intent)
            } catch (_: ActivityNotFoundException) {
            }
        }
    }

    private fun ensureLocationPermission(): Boolean {
        val fine = ContextCompat.checkSelfPermission(this, Manifest.permission.ACCESS_FINE_LOCATION) ==
            PackageManager.PERMISSION_GRANTED
        val coarse = ContextCompat.checkSelfPermission(this, Manifest.permission.ACCESS_COARSE_LOCATION) ==
            PackageManager.PERMISSION_GRANTED
        if (fine || coarse) {
            return true
        }
        val latch = CountDownLatch(1)
        placeGranted = false
        runOnUiThread {
            placeWaiting = latch
            try {
                placePermission.launch(
                    arrayOf(
                        Manifest.permission.ACCESS_FINE_LOCATION,
                        Manifest.permission.ACCESS_COARSE_LOCATION,
                    ),
                )
            } catch (_: IllegalStateException) {
                placeGranted = false
                latch.countDown()
            }
        }
        latch.await(60, TimeUnit.SECONDS)
        return placeGranted
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

    fun beginFilm(): Int {
        if (scanner != null) {
            return 1
        }
        val latch = CountDownLatch(1)
        val result = AtomicInteger(1)
        runOnUiThread {
            val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
                PackageManager.PERMISSION_GRANTED
            if (granted) {
                startFilm(result, latch)
            } else {
                filmResult = result
                filmLatch = latch
                try {
                    filmPermission.launch(Manifest.permission.CAMERA)
                } catch (_: IllegalStateException) {
                    latch.countDown()
                }
            }
        }
        latch.await(60, TimeUnit.SECONDS)
        return result.get()
    }

    fun endFilm() {
        runOnUiThread {
            film?.stop()
            film = null
        }
    }

    fun decodeFilm(frame: ByteArray): ByteArray? = film?.decode(frame)

    private fun startFilm(result: AtomicInteger, latch: CountDownLatch) {
        film?.stop()
        val next = Film(this)
        next.start { ok ->
            if (ok) {
                film = next
                result.set(0)
            } else {
                next.stop()
                film = null
            }
            latch.countDown()
        }
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

        private val NOTE_IDS = AtomicInteger(0)

        @Volatile
        private var askedPower = false

        @Volatile
        private var instance: MainActivity? = null

        @Volatile
        private var process: Context? = null

        @JvmStatic
        fun onScreen(): Boolean = instance?.showing() == true

        @JvmStatic
        fun postNote(name: String, words: String) {
            val context = process ?: return
            if (Build.VERSION.SDK_INT >= 33 &&
                ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
                PackageManager.PERMISSION_GRANTED
            ) {
                return
            }
            val manager = context.getSystemService(NotificationManager::class.java) ?: return
            val channelId = "notes"
            val channel = NotificationChannel(
                channelId,
                "Notes",
                NotificationManager.IMPORTANCE_HIGH,
            )
            manager.createNotificationChannel(channel)
            val launch = Intent(context, MainActivity::class.java).apply {
                flags = Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP
            }
            val pending = PendingIntent.getActivity(
                context,
                0,
                launch,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            val icon = org.personal.prns.comms.R.drawable.ic_stat_n10a
            val title = name.ifBlank { "Someone" }
            val text = words.ifBlank { "A new note" }
            val notification = Notification.Builder(context, channelId)
                .setSmallIcon(icon)
                .setContentTitle(title)
                .setContentText(text)
                .setStyle(Notification.BigTextStyle().bigText(text))
                .setAutoCancel(true)
                .setContentIntent(pending)
                .build()
            manager.notify(NOTE_IDS.incrementAndGet(), notification)
        }

        @JvmStatic
        fun showPlace(query: String) {
            instance?.openPlace(query)
        }

        @JvmStatic
        fun chooseFile(maxBytes: Long): String? = instance?.pickFile(maxBytes)

        @JvmStatic
        fun saveDownload(sourcePath: String, displayName: String): String? =
            instance?.writeDownload(sourcePath, displayName)

        @JvmStatic
        fun openDownload(displayName: String) {
            val activity = instance ?: return
            activity.runOnUiThread { activity.viewDownload(displayName) }
        }

        @JvmStatic
        fun whereAmI(): String? {
            val activity = instance ?: return null
            return activity.readPlace()
        }

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
