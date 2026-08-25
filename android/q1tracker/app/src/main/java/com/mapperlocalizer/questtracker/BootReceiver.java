package com.mapperlocalizer.questtracker;

/*
 * Start the tracker on boot without root.
 *
 * The rooted path (tools/quest_boot_tracker.sh as a Magisk service) is strictly
 * more capable -- it can also defeat the proximity sensor and dismiss the USB
 * dialog, and it is not on a deadline. This exists for the headsets where root
 * is not available at all: the Quest 1's bootloader is locked, so a Magisk
 * service is not an option there.
 *
 * Two things measured on a real boot shape this code:
 *
 *  - "Is the app process alive" is NOT a usable running-check. Hosting this
 *    broadcast is itself what starts the process, so the naive check is true
 *    before anything has launched -- it reported success on a boot where
 *    MainActivity never started at all. Hence MainActivity.sActivityRunning,
 *    which the receiver can read because it runs in that same process.
 *  - A receiver's process is killable as soon as onReceive returns, so a
 *    minutes-long retry loop posted to a Handler is not guaranteed to survive.
 *    goAsync() holds the process up while the broadcast is in flight; the
 *    budget below stays inside that window rather than pretending to be longer.
 *
 * The honest limit: if vrshell's RequiresControllersLaunchInterceptor is still
 * refusing at the end of the budget -- controllers asleep or charging -- this
 * gives up, and the user has to wake a controller. Root does not share that
 * deadline.
 */

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

public class BootReceiver extends BroadcastReceiver {
    private static final String TAG = "QuestTracker";
    // Stay inside goAsync()'s tolerance: past roughly a minute the system may
    // complain or kill us outright, and an ANR helps nobody.
    private static final long BUDGET_MS = 55_000;
    private static final long RETRY_MS = 5_000;
    // The VR runtime is not ready the instant BOOT_COMPLETED lands; a start
    // fired into a half-initialised shell just fails.
    private static final long INITIAL_DELAY_MS = 15_000;

    @Override public void onReceive(Context ctx, Intent intent) {
        String a = intent == null ? null : intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(a)
                && !"android.intent.action.QUICKBOOT_POWERON".equals(a)) {
            return;
        }
        final Context app = ctx.getApplicationContext();
        final PendingResult pr = goAsync();
        new Thread(new Runnable() {
            @Override public void run() {
                try {
                    Log.i(TAG, "boot received; starting the tracker");
                    sleep(INITIAL_DELAY_MS);
                    long deadline = android.os.SystemClock.elapsedRealtime() + BUDGET_MS;
                    int tries = 0;
                    while (android.os.SystemClock.elapsedRealtime() < deadline) {
                        if (MainActivity.sActivityRunning) {
                            Log.i(TAG, "tracker started after boot (try " + tries + ")");
                            return;
                        }
                        tries++;
                        try {
                            Intent i = new Intent(app, MainActivity.class);
                            i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                            app.startActivity(i);
                        } catch (Throwable t) {
                            Log.w(TAG, "boot start try " + tries + ": " + t);
                        }
                        sleep(RETRY_MS);
                    }
                    Log.w(TAG, "tracker did not start within " + (BUDGET_MS / 1000)
                               + "s after " + tries + " tries; wake a controller "
                               + "and launch it, or use the rooted boot service");
                } finally {
                    pr.finish();
                }
            }
        }, "boot-start").start();
    }

    private static void sleep(long ms) {
        try { Thread.sleep(ms); } catch (InterruptedException ignored) { }
    }
}
