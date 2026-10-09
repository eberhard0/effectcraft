package com.iameberhard.effectcraft

import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity

/**
 * The Android shell around the Rust app (`apps/effectcraft-android`, loaded as
 * `libeffectcraft_android.so` by GameActivity's glue). The Rust side calls [pickOpen],
 * [saveToDownloads] and [openUrl] through JNI; picked files go back through [nativeDeliverFile].
 */
class MainActivity : GameActivity() {

    companion object {
        private const val TAG = "effectcraft"
        private const val FOLDER = "EffectCraft"

        init {
            System.loadLibrary("effectcraft_android")
        }
    }

    /** Implemented in Rust: hands a picked file (name, contents) to the app. */
    private external fun nativeDeliverFile(name: String, bytes: ByteArray)

    /** Files this session created in Downloads, so saving the same name again overwrites. */
    private val savedUris = HashMap<String, Uri>()

    /** File › Import lets the user pick several media files at once; Open Project takes the first. */
    private val openDocuments = registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris: List<Uri> ->
        if (uris.isEmpty()) return@registerForActivityResult
        // Read off the UI thread; a video can take a while.
        Thread {
            for (uri in uris) {
                try {
                    val name = displayName(uri) ?: "file"
                    val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                    if (bytes == null) {
                        toast("Couldn't read $name")
                    } else {
                        nativeDeliverFile(name, bytes)
                    }
                } catch (e: Exception) {
                    Log.e(TAG, "open failed", e)
                    toast("Couldn't open the file: ${e.message}")
                }
            }
        }.start()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        hideSystemBars()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    /** Full screen for the canvas; a swipe from an edge shows the bars briefly. */
    private fun hideSystemBars() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    // ---- Called from Rust (any thread) --------------------------------------------------------

    /** File › Open / Import: the system picker. All files, since .ecproj projects have no MIME type. */
    fun pickOpen() {
        runOnUiThread {
            try {
                openDocuments.launch(arrayOf("*/*"))
            } catch (e: Exception) {
                Log.e(TAG, "picker failed", e)
                toast("Couldn't open the file picker: ${e.message}")
            }
        }
    }

    /**
     * Render Queue outputs and saved projects: write [bytes] as `Downloads/EffectCraft/[name]`
     * through MediaStore. The same name saved again in this session overwrites the file.
     * Returns null on success, else a message.
     */
    fun saveToDownloads(name: String, bytes: ByteArray): String? {
        return try {
            val resolver = contentResolver
            val existing = savedUris[name]
            val uri = existing ?: run {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, name)
                    put(MediaStore.Downloads.MIME_TYPE, mimeFor(name))
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/" + FOLDER)
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return "couldn't create $name in Downloads"
            }
            val out = resolver.openOutputStream(uri, "wt") ?: return "couldn't open $name for writing"
            out.use { it.write(bytes) }
            if (existing == null) {
                resolver.update(uri, ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }, null, null)
                savedUris[name] = uri
            }
            toast("Saved to Downloads/$FOLDER/$name")
            null
        } catch (e: Exception) {
            Log.e(TAG, "save failed", e)
            e.message ?: e.toString()
        }
    }

    /** Help menu links. */
    fun openUrl(url: String) {
        runOnUiThread {
            try {
                startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
            } catch (e: Exception) {
                Log.e(TAG, "open url failed", e)
                toast("Couldn't open $url")
            }
        }
    }

    // ---- Helpers ------------------------------------------------------------------------------

    private fun displayName(uri: Uri): String? {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (i >= 0) return c.getString(i)
            }
        }
        return uri.lastPathSegment
    }

    private fun mimeFor(name: String): String = when (name.substringAfterLast('.', "").lowercase()) {
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "tif", "tiff" -> "image/tiff"
        "webp" -> "image/webp"
        "gif" -> "image/gif"
        "bmp" -> "image/bmp"
        "mp4", "m4v" -> "video/mp4"
        "mov" -> "video/quicktime"
        "webm" -> "video/webm"
        "mkv" -> "video/x-matroska"
        "wav" -> "audio/wav"
        "mp3" -> "audio/mpeg"
        "flac" -> "audio/flac"
        "ogg", "opus" -> "audio/ogg"
        "aac", "m4a" -> "audio/mp4"
        "json" -> "application/json"
        else -> "application/octet-stream"
    }

    private fun toast(text: String) {
        runOnUiThread { Toast.makeText(this, text, Toast.LENGTH_SHORT).show() }
    }
}
