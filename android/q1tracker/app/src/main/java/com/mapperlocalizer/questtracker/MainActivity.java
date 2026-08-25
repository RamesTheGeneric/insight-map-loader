package com.mapperlocalizer.questtracker;

/*
 * Thin USB shim over the native OpenXR tracker.
 *
 * quest_tracker.cpp (android_main, via NativeActivity) owns everything that
 * matters: the OpenXR pose loop, MPT1 output, and -- when a camera is attached
 * -- the UVC capture + posed-frame stream. The one thing native code cannot do
 * on Android is OPEN a USB device: only the Java UsbManager can, and it hands
 * back a file descriptor that libusb wraps without root or enumeration. This
 * class exists solely to get that fd and pass it down via nativeSetCameraFd().
 *
 * Extending NativeActivity (rather than replacing it) keeps android_main and the
 * whole tracker unchanged; the camera is purely additive. A headset with no
 * camera never reaches tryOpen() and behaves exactly as before.
 *
 * The connection field is deliberately long-lived: libusb_wrap_sys_device()
 * borrows the fd, so closing the UsbDeviceConnection would invalidate it
 * mid-capture.
 */

import android.app.NativeActivity;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.hardware.usb.UsbConstants;
import android.hardware.usb.UsbDevice;
import android.hardware.usb.UsbDeviceConnection;
import android.hardware.usb.UsbInterface;
import android.hardware.usb.UsbManager;
import android.net.wifi.WifiManager;
import android.os.BatteryManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.PowerManager;
import android.util.Log;
import android.view.InputDevice;

public class MainActivity extends NativeActivity {
    static { System.loadLibrary("questtracker"); }

    private static final String TAG = "QuestTracker";
    private static final String ACTION_PERM = "com.mapperlocalizer.questtracker.USB_PERMISSION";

    private UsbManager usb;
    private UsbDeviceConnection conn;   // kept open for the capture's lifetime

    // Without these the headset power-manages the radio away in standby:
    // observed as NETWORK_DISCONNECTION_EVENT with screen=off, after which it
    // does NOT reassociate while idle and every pose is sent into a void. A
    // tracker has to stay on the network for as long as it is tracking, so hold
    // both locks for the life of the activity. HIGH_PERF also disables WiFi
    // power-save, which is what introduces latency spikes on an idle link --
    // and pose timing is the thing this whole pipeline is most sensitive to.
    private WifiManager.WifiLock wifiLock;
    private PowerManager.WakeLock wakeLock;

    /** Hand the opened UVC file descriptor to native capture; fd < 0 = detached. */
    private native void nativeSetCameraFd(int fd);

    /** Battery for an MPT1 slot (0 waist / 1 left / 2 right); pct 0 = unknown. */
    private native void nativeSetBattery(int device, int pct, boolean charging);

    /** Give the pose path a BtServer to also push packets into; null = off. */
    private native void nativeSetBtServer(Object btServer);

    /** Consecutive poses that could not be sent because there is no route. */
    private native int nativeSendFailures();
    /** The Wi-Fi the tracker belongs on, from config.txt (may be empty). */
    private native String nativeWifiSsid();
    private native String nativeWifiPass();

    private final Handler netHandler = new Handler(Looper.getMainLooper());
    private static final long NET_PERIOD_MS = 5_000;
    // ~2 s of poses at 72 Hz. High enough that a momentary blip is ignored, low
    // enough that a real disconnection is caught within one check.
    private static final int FAIL_THRESHOLD = 150;
    private long lastRejoinMs;

    private BtServer bt;

    private final Handler battHandler = new Handler(Looper.getMainLooper());
    private static final long BATT_PERIOD_MS = 30_000;   // it moves slowly

    private final BroadcastReceiver receiver = new BroadcastReceiver() {
        @Override public void onReceive(Context c, Intent i) {
            String a = i.getAction();
            if (ACTION_PERM.equals(a)) {
                if (i.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                    UsbDevice d = i.getParcelableExtra(UsbManager.EXTRA_DEVICE);
                    if (d != null) open(d);
                } else {
                    Log.e(TAG, "USB permission denied");
                }
            } else if (UsbManager.ACTION_USB_DEVICE_DETACHED.equals(a)) {
                UsbDevice d = i.getParcelableExtra(UsbManager.EXTRA_DEVICE);
                if (d != null && conn != null) {
                    Log.i(TAG, "camera detached");
                    nativeSetCameraFd(-1);
                    conn.close();
                    conn = null;
                }
            }
        }
    };

    /** True while the tracking activity is alive. BootReceiver polls this: it
     *  runs in this same process, and "is the process alive" is useless there
     *  because hosting the boot broadcast is itself what starts the process. */
    static volatile boolean sActivityRunning = false;

    @Override protected void onCreate(Bundle b) {
        super.onCreate(b);   // starts android_main + loads the native lib
        sActivityRunning = true;
        keepAwakeWithoutBeingWorn();
        usb = (UsbManager) getSystemService(Context.USB_SERVICE);
        acquireLocks();
        battHandler.post(battPoll);
        netHandler.postDelayed(netWatchdog, NET_PERIOD_MS);
        maybeStartBluetooth();

        IntentFilter f = new IntentFilter(ACTION_PERM);
        f.addAction(UsbManager.ACTION_USB_DEVICE_DETACHED);
        // API 33+ requires an explicit export flag on runtime receivers.
        if (android.os.Build.VERSION.SDK_INT >= 33)
            registerReceiver(receiver, f, Context.RECEIVER_NOT_EXPORTED);
        else
            registerReceiver(receiver, f);

        // Attach-launch delivers the device in the intent (permission implicit).
        UsbDevice fromIntent = getIntent() != null
                ? (UsbDevice) getIntent().getParcelableExtra(UsbManager.EXTRA_DEVICE) : null;
        if (fromIntent != null) tryOpen(fromIntent);
        else findAndOpen();   // already-attached case (e.g. adb-launched)
    }

    @Override protected void onNewIntent(Intent i) {
        super.onNewIntent(i);
        UsbDevice d = i != null ? (UsbDevice) i.getParcelableExtra(UsbManager.EXTRA_DEVICE) : null;
        if (d != null) tryOpen(d);
    }

    // Poll battery for the headset and both controllers.
    //
    // The headset is a plain BatteryManager read. The controllers are not: they
    // surface as Android input devices (INPUT_DEVICE_CLASS_VR_PERIPHERAL), so
    // their charge comes from InputDevice.getBatteryState, which only exists on
    // API 34+. Left/right is decided by the device's source/descriptor, and the
    // pairing is stable for a session -- but if it ever mismatches, the symptom
    // is two correct percentages on swapped trackers, not a wrong number.
    private final Runnable battPoll = new Runnable() {
        @Override public void run() {
            try {
                BatteryManager bm = (BatteryManager) getSystemService(Context.BATTERY_SERVICE);
                if (bm != null) {
                    int pct = bm.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY);
                    boolean chg = bm.isCharging();
                    if (pct >= 0 && pct <= 100) nativeSetBattery(0, pct, chg);
                }
                if (android.os.Build.VERSION.SDK_INT >= 34) pollControllerBatteries();
            } catch (Exception e) {
                Log.e(TAG, "battery poll: " + e);
            }
            battHandler.postDelayed(this, BATT_PERIOD_MS);
        }
    };

    private void pollControllerBatteries() {
        for (int id : InputDevice.getDeviceIds()) {
            InputDevice d = InputDevice.getDevice(id);
            if (d == null) continue;
            // Only hand controllers; skip the headset's own buttons etc.
            if ((d.getSources() & InputDevice.SOURCE_JOYSTICK) == 0) continue;
            int slot = -1;
            String name = d.getName() == null ? "" : d.getName().toLowerCase();
            if (name.contains("left")) slot = 1;
            else if (name.contains("right")) slot = 2;
            else {
                // Meta names them by hex id, not handedness -- fall back to
                // enumeration order, which is stable within a session.
                slot = (slot == -1) ? (nextCtrlSlot <= 2 ? nextCtrlSlot++ : -1) : slot;
            }
            if (slot < 1 || slot > 2) continue;
            try {
                android.hardware.BatteryState bs = d.getBatteryState();
                if (bs != null && bs.isPresent()) {
                    float cap = bs.getCapacity();       // 0..1, NaN if unknown
                    if (!Float.isNaN(cap) && cap >= 0f) {
                        boolean chg = bs.getStatus()
                                == android.os.BatteryManager.BATTERY_STATUS_CHARGING;
                        nativeSetBattery(slot, Math.round(cap * 100f), chg);
                    }
                }
            } catch (Throwable ignored) { /* older/odd runtimes */ }
        }
        nextCtrlSlot = 1;   // reset for the next sweep
    }

    private int nextCtrlSlot = 1;

    // Bluetooth is opt-in via "bt=1" in the same config.txt the native side
    // reads, so enabling it needs no rebuild. Read here rather than passed down
    // from native because the socket lives in Java anyway.
    private void maybeStartBluetooth() {
        try {
            java.io.File f = new java.io.File(getExternalFilesDir(null), "config.txt");
            boolean want = false;
            if (f.exists()) {
                java.io.BufferedReader r = new java.io.BufferedReader(new java.io.FileReader(f));
                String line;
                while ((line = r.readLine()) != null) {
                    String s = line.trim();
                    if (s.startsWith("bt=")) want = !s.substring(3).trim().equals("0");
                }
                r.close();
            }
            if (!want) { Log.i(TAG, "BT transport disabled (set bt=1 in config.txt)"); return; }
            bt = new BtServer();
            bt.start();
            nativeSetBtServer(bt);
        } catch (Exception e) {
            Log.e(TAG, "BT start: " + e);
        }
    }

    // Rejoin the tracker network when poses stop being routable.
    //
    // A dedicated tracker AP has no uplink, so Android's HTTPS validation probe
    // can never pass and the network is never marked VALIDATED. With the house
    // Wi-Fi also saved and validated, Android prefers it -- so any dongle restart
    // leaves the tracker happily connected to the wrong network, still emitting
    // poses at 72 Hz into a void, with nothing in the app that would notice.
    //
    // Detection is deliberately sendto()-based rather than SSID-based: reading
    // the current SSID needs location permission on modern Android, whereas
    // ENETUNREACH is exactly what a vanished route looks like and costs nothing.
    private final Runnable netWatchdog = new Runnable() {
        @Override public void run() {
            try {
                int fails = nativeSendFailures();
                if (fails >= FAIL_THRESHOLD) {
                    long now = System.currentTimeMillis();
                    // Rate-limit: a suggestion takes time to act on, and
                    // re-suggesting every tick would just churn.
                    if (now - lastRejoinMs > 20_000) {
                        lastRejoinMs = now;
                        Log.w(TAG, "no route for " + fails + " poses; requesting "
                                   + "the tracker network again");
                        suggestTrackerNetwork();
                    }
                }
            } catch (Throwable t) {
                Log.e(TAG, "net watchdog: " + t);
            }
            netHandler.postDelayed(this, NET_PERIOD_MS);
        }
    };

    /**
     * Ask Android to join the tracker Wi-Fi.
     *
     * Uses the suggestion API because an ordinary app cannot force a connection
     * on Android 10+. This is a nudge, not a command: the platform still scores
     * networks and may prefer a validated one. The robust deployment answer is
     * to not have the house Wi-Fi saved on a dedicated tracker at all -- this
     * covers the common case where the AP simply went away and came back.
     */
    private void suggestTrackerNetwork() {
        String ssid = nativeWifiSsid();
        String pass = nativeWifiPass();
        if (ssid == null || ssid.isEmpty()) {
            Log.w(TAG, "no wifi_ssid in config.txt; cannot rejoin automatically");
            return;
        }
        try {
            WifiManager wm = (WifiManager)
                    getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            if (wm == null) return;
            android.net.wifi.WifiNetworkSuggestion.Builder b =
                    new android.net.wifi.WifiNetworkSuggestion.Builder().setSsid(ssid);
            if (pass != null && !pass.isEmpty()) b.setWpa2Passphrase(pass);
            // Untrusted networks are eligible even without internet, which is
            // exactly what a tracker AP is.
            b.setIsAppInteractionRequired(false);
            java.util.List<android.net.wifi.WifiNetworkSuggestion> list =
                    java.util.Collections.singletonList(b.build());
            wm.removeNetworkSuggestions(list);       // re-adding re-triggers it
            int rc = wm.addNetworkSuggestions(list);
            Log.i(TAG, "suggested '" + ssid + "' -> status " + rc);
        } catch (Throwable t) {
            Log.e(TAG, "suggest network: " + t);
        }
    }

    private void acquireLocks() {
        try {
            WifiManager wm = (WifiManager)
                    getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            if (wm != null) {
                wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF,
                                             TAG + ":net");
                wifiLock.setReferenceCounted(false);
                wifiLock.acquire();
            }
            PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
            if (pm != null) {
                wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, TAG + ":cpu");
                wakeLock.setReferenceCounted(false);
                wakeLock.acquire();   // no timeout: it is released in onDestroy
            }
            Log.i(TAG, "wifi/wake locks held (wifi=" + (wifiLock != null)
                       + " wake=" + (wakeLock != null) + ")");
        } catch (Exception e) {
            Log.e(TAG, "lock acquire failed: " + e);
        }
    }

    private void releaseLocks() {
        try { if (wifiLock != null && wifiLock.isHeld()) wifiLock.release(); } catch (Exception ignored) {}
        try { if (wakeLock != null && wakeLock.isHeld()) wakeLock.release(); } catch (Exception ignored) {}
        wifiLock = null;
        wakeLock = null;
    }

    /* A tracker is worn on the waist, so the headset's proximity sensor always
     * reads "not worn" and the runtime never gives this app focus -- measured:
     * "SetFocusedClient: Updating Focus State: 0", zero MPT1 packets, until a
     * prox_close broadcast arrives. adb can send it, but an unattended tracker
     * has no host, so try to send it from here. It is not in the protected
     * broadcast list, so this may legitimately work; if the platform disagrees
     * it throws SecurityException and the fallback is the one-time host setup in
     * tools/quest_autostart_setup.sh (or covering the sensor with tape). */
    private void keepAwakeWithoutBeingWorn() {
        try {
            sendBroadcast(new Intent("com.oculus.vrpowermanager.prox_close"));
            Log.i(TAG, "sent prox_close");
        } catch (Throwable t) {
            Log.w(TAG, "prox_close refused (" + t + "); run "
                       + "tools/quest_autostart_setup.sh from a host, or tape "
                       + "over the proximity sensor");
        }
    }

    @Override protected void onDestroy() {
        sActivityRunning = false;
        battHandler.removeCallbacks(battPoll);
        netHandler.removeCallbacks(netWatchdog);
        try { nativeSetBtServer(null); } catch (Throwable ignored) {}
        if (bt != null) { bt.stop(); bt = null; }
        releaseLocks();
        try { unregisterReceiver(receiver); } catch (Exception ignored) {}
        if (conn != null) { nativeSetCameraFd(-1); conn.close(); conn = null; }
        super.onDestroy();
    }

    private void findAndOpen() {
        if (usb == null) return;
        for (UsbDevice d : usb.getDeviceList().values()) {
            if (isVideo(d)) { tryOpen(d); return; }
        }
        Log.i(TAG, "no UVC camera attached (tracker-only mode)");
    }

    private void tryOpen(UsbDevice d) {
        if (!isVideo(d)) return;
        if (usb.hasPermission(d)) {
            open(d);
        } else {
            // On attach-launch this path is not hit (permission is implicit);
            // it is the fallback for an adb-launched, already-attached camera.
            Log.i(TAG, "requesting USB permission for " + d.getDeviceName());
            PendingIntent pi = PendingIntent.getBroadcast(this, 0,
                    new Intent(ACTION_PERM).setPackage(getPackageName()),
                    PendingIntent.FLAG_MUTABLE);
            usb.requestPermission(d, pi);
        }
    }

    private void open(UsbDevice d) {
        UsbDeviceConnection c = usb.openDevice(d);
        if (c == null) { Log.e(TAG, "openDevice failed for " + d.getDeviceName()); return; }
        if (conn != null) conn.close();
        conn = c;
        int fd = c.getFileDescriptor();
        Log.i(TAG, "camera opened " + d.getDeviceName() + " fd=" + fd);
        nativeSetCameraFd(fd);
    }

    private static boolean isVideo(UsbDevice d) {
        if (d.getDeviceClass() == UsbConstants.USB_CLASS_VIDEO
                || d.getDeviceClass() == UsbConstants.USB_CLASS_MISC) return true;
        for (int i = 0; i < d.getInterfaceCount(); i++) {
            UsbInterface intf = d.getInterface(i);
            if (intf.getInterfaceClass() == UsbConstants.USB_CLASS_VIDEO) return true;
        }
        return false;
    }
}
