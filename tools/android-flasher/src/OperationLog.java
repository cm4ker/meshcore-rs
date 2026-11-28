package ru.ommesh.otg;

import android.Manifest;
import android.content.ContentResolver;
import android.content.ContentValues;
import android.content.Context;
import android.content.pm.PackageManager;
import android.media.MediaScannerConnection;
import android.net.Uri;
import android.os.Build;
import android.os.Environment;
import android.provider.MediaStore;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;
import java.util.UUID;

final class OperationLog {
    static boolean canSave(Context context) {
        return Build.VERSION.SDK_INT >= 29 || context.checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE)
            == PackageManager.PERMISSION_GRANTED;
    }

    static String save(Context context, String transcript) throws IOException {
        Date now = new Date();
        String stamp = new SimpleDateFormat("yyyyMMdd-HHmmss-SSS", Locale.ROOT).format(now);
        String name = "ota-app-" + stamp + "-" + UUID.randomUUID().toString().substring(0, 6) + ".txt";
        String date = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.ROOT).format(now);
        byte[] data = ("ota-app 1.3.2\n" + date + "\n" + transcript).getBytes(StandardCharsets.UTF_8);
        if (Build.VERSION.SDK_INT >= 29) {
            saveToMediaStore(context.getContentResolver(), name, data);
        } else {
            File directory = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS);
            if (!directory.isDirectory() && !directory.mkdirs() && !directory.isDirectory()) {
                throw new IOException("Папка загрузок недоступна.");
            }
            File log = new File(directory, name);
            try (OutputStream output = new FileOutputStream(log)) { output.write(data); }
            MediaScannerConnection.scanFile(context, new String[]{log.getPath()}, new String[]{"text/plain"}, null);
        }
        return name;
    }

    private static void saveToMediaStore(ContentResolver resolver, String name, byte[] data) throws IOException {
        ContentValues values = new ContentValues();
        values.put(MediaStore.Downloads.DISPLAY_NAME, name);
        values.put(MediaStore.Downloads.MIME_TYPE, "text/plain");
        values.put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/");
        values.put(MediaStore.Downloads.IS_PENDING, 1);
        Uri collection = MediaStore.Downloads.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY);
        Uri uri = resolver.insert(collection, values);
        if (uri == null) throw new IOException("Папка загрузок недоступна.");
        try {
            try (OutputStream output = resolver.openOutputStream(uri, "w")) {
                if (output == null) throw new IOException("Не удалось открыть файл журнала.");
                output.write(data);
            }
            ContentValues ready = new ContentValues();
            ready.put(MediaStore.Downloads.IS_PENDING, 0);
            if (resolver.update(uri, ready, null, null) == 0) throw new IOException("Не удалось сохранить файл журнала.");
        } catch (IOException | RuntimeException e) {
            try { resolver.delete(uri, null, null); } catch (RuntimeException cleanup) { e.addSuppressed(cleanup); }
            throw e;
        }
    }
}
