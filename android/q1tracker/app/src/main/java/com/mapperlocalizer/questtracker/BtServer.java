package com.mapperlocalizer.questtracker;

/*
 * Bluetooth transport for MPT1 poses, as an alternative to UDP.
 *
 * Classic RFCOMM (SPP), not BLE. BLE's throughput would just about carry the
 * ~117 kbps this needs, but it delivers in bursts quantised to the connection
 * interval (>= 7.5 ms), which is exactly the kind of timing quantisation the
 * pose pipeline is most sensitive to. RFCOMM is a plain stream and pairs with a
 * virtual COM port on Windows, which the PC-side bridge can read directly.
 *
 * Framing: raw 68-byte MPT1 packets back to back. The reader resynchronises on
 * the magic, so a mid-stream connect cannot desync permanently.
 *
 * TIMING CAVEAT, stated plainly: Bluetooth adds more latency AND more jitter
 * than the Wi-Fi path. The driver's clock-offset estimator (see
 * mapper_protocol.h t_ns) absorbs the constant component, because poses are
 * aged from the producer's own timestamp rather than from arrival -- but it
 * cannot absorb jitter, which becomes extrapolation error. Expect this to be
 * smooth-ish and robust, not better than UDP.
 *
 * Writes happen on a dedicated thread off a bounded queue: a stalled BT link
 * must never block the OpenXR frame loop, and dropping stale poses is always
 * better than delaying fresh ones.
 */

import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothServerSocket;
import android.bluetooth.BluetoothSocket;
import android.util.Log;

import java.io.OutputStream;
import java.util.UUID;
import java.util.concurrent.ArrayBlockingQueue;

public class BtServer {
    private static final String TAG = "QuestTracker";
    private static final String NAME = "MapperMPT1";
    // Standard Serial Port Profile UUID: pairs as a COM port on Windows with no
    // custom driver, which is what keeps the PC side trivial.
    private static final UUID SPP = UUID.fromString("00001101-0000-1000-8000-00805F9B34FB");

    // Small on purpose. If the link cannot keep up we want the newest poses,
    // not a backlog of old ones arriving late.
    private final ArrayBlockingQueue<byte[]> queue = new ArrayBlockingQueue<>(64);
    private volatile boolean running = false;
    private Thread acceptThread, writeThread;
    private BluetoothServerSocket server;
    private volatile BluetoothSocket client;

    public boolean isConnected() { return client != null; }

    public void start() {
        if (running) return;
        BluetoothAdapter ad = BluetoothAdapter.getDefaultAdapter();
        if (ad == null || !ad.isEnabled()) {
            Log.e(TAG, "BT: adapter unavailable or off");
            return;
        }
        running = true;
        acceptThread = new Thread(this::acceptLoop, "mpt1-bt-accept");
        acceptThread.setDaemon(true);
        acceptThread.start();
        writeThread = new Thread(this::writeLoop, "mpt1-bt-write");
        writeThread.setDaemon(true);
        writeThread.start();
        Log.i(TAG, "BT: SPP server starting (" + NAME + ")");
    }

    public void stop() {
        running = false;
        closeClient();
        try { if (server != null) server.close(); } catch (Exception ignored) {}
        queue.clear();
    }

    /** Called from the pose path. Never blocks: drops the oldest on overflow. */
    public void send(byte[] packet) {
        if (!running || client == null) return;
        if (!queue.offer(packet)) {
            queue.poll();          // discard the stalest
            queue.offer(packet);
        }
    }

    private void acceptLoop() {
        while (running) {
            try {
                BluetoothAdapter ad = BluetoothAdapter.getDefaultAdapter();
                server = ad.listenUsingRfcommWithServiceRecord(NAME, SPP);
                Log.i(TAG, "BT: waiting for a PC to connect");
                BluetoothSocket s = server.accept();       // blocks
                try { server.close(); } catch (Exception ignored) {}
                closeClient();
                client = s;
                Log.i(TAG, "BT: connected to "
                           + (s.getRemoteDevice() != null ? s.getRemoteDevice().getName() : "?"));
            } catch (Exception e) {
                if (running) Log.e(TAG, "BT accept: " + e);
                try { Thread.sleep(2000); } catch (InterruptedException ignored) {}
            }
        }
    }

    private void writeLoop() {
        while (running) {
            BluetoothSocket s = client;
            if (s == null) {
                try { Thread.sleep(100); } catch (InterruptedException ignored) {}
                continue;
            }
            try {
                OutputStream os = s.getOutputStream();
                while (running && client == s) {
                    byte[] p = queue.poll(500, java.util.concurrent.TimeUnit.MILLISECONDS);
                    if (p == null) continue;
                    os.write(p);
                    os.flush();     // send now; buffering would add latency
                }
            } catch (Exception e) {
                Log.e(TAG, "BT write: " + e + " (will re-accept)");
                closeClient();
            }
        }
    }

    private void closeClient() {
        BluetoothSocket s = client;
        client = null;
        try { if (s != null) s.close(); } catch (Exception ignored) {}
    }
}
