/*
 * quest_tracker.cpp -- publish a Quest headset's own 6DoF pose as an MPT1
 * tracker, so the SteamVR driver can expose it and SpaceCalibrator can align
 * the Quest's tracking space with the SteamVR universe.
 *
 * This deliberately uses the SUPPORTED path: the headset's inside-out pose via
 * OpenXR. No camera access, no root -- Quest 1 never exposed raw tracking
 * frames, but its own pose is a first-class API.
 *
 * The session renders nothing. OpenXR still requires the frame loop
 * (xrWaitFrame/xrBeginFrame/xrEndFrame) to run for the session to reach a
 * state where poses are valid, so we submit zero layers each frame -- legal,
 * and it keeps the compositor cost near zero. Without lenses that is exactly
 * what we want: a tracking puck, not a display.
 *
 * Reference space is LOCAL, not STAGE: STAGE depends on a configured guardian,
 * and the plan is to run with the boundary disabled. LOCAL's origin is
 * arbitrary (headset pose at session start), which does not matter -- resolving
 * an arbitrary origin against the SteamVR universe is precisely SpaceCalibrator's
 * job.
 *
 * Config is read from <externalFilesDir>/config.txt so the target can change
 * without a rebuild:
 *     host=192.168.1.113
 *     port=5180
 *     device=0
 *     mirror=192.168.1.138:5181
 */
#include <android/log.h>
#include <android_native_app_glue.h>
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <sys/socket.h>
#include <unistd.h>

#include <jni.h>

#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <ctime>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#if Q1T_UVC
extern "C" {
#include "uvc_android.h"
}
#include "posed_frame.h"
#endif  // Q1T_UVC

#define XR_USE_PLATFORM_ANDROID
#define XR_USE_GRAPHICS_API_OPENGL_ES
// Exposes XR_KHR_convert_timespec_time in openxr_platform.h; without it the
// extension name and xrConvertTimespecTimeToTimeKHR are compiled out.
#define XR_USE_TIMESPEC
#include <EGL/egl.h>
#include <GLES3/gl3.h>
#include <openxr/openxr.h>
#include <openxr/openxr_platform.h>

#define TAG "QuestTracker"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, TAG, __VA_ARGS__)

namespace {

struct Config {
    std::string host = "192.168.1.113";
    int port = 5180;
    int device = 0;              // MPT1 device id (0 waist / 1 left / 2 right)
    std::string mirrorHost;
    int mirrorPort = 0;

    // The Wi-Fi the tracker belongs on. Needed because a dedicated tracker AP
    // has no internet, so Android never marks it VALIDATED and prefers the
    // house network whenever both are in range -- meaning a dongle restart
    // silently strands the tracker on the wrong network.
    std::string wifiSsid;
    std::string wifiPass;

    // Touch controllers as extra trackers. Quest Pro controllers are
    // self-tracked (own cameras + IMU), so they keep tracking with no headset
    // line-of-sight -- which is what makes them usable strapped to a body.
    // MPT1 only has three slots, and HMD/left/right maps onto them exactly.
    bool ctrlEnable = true;
    int ctrlLeftDevice = 1;      // MAPPER_DEV_LEFT_FOOT
    int ctrlRightDevice = 2;     // MAPPER_DEV_RIGHT_FOOT

    // Known-pose mapping camera (a UVC module bolted to the Quest). Off unless
    // cam=1, so a headset with no camera behaves exactly as the tracker did.
    bool camEnable = false;
    std::string camHost;         // defaults to `host` if left blank
    int camPort = 5171;          // MPF1 posed-frame TCP (distinct from MPT1 5180)
    bool camMjpeg = true;        // module streams MJPEG; sent without re-encode
    int camW = 640;
    int camH = 480;
    int camFps = 30;
};

Config g_cfg;

void loadConfig(const char *dir)
{
    char path[512];
    snprintf(path, sizeof path, "%s/config.txt", dir);
    FILE *f = fopen(path, "r");
    if (!f) { LOGI("no %s, using defaults", path); return; }
    char line[256];
    while (fgets(line, sizeof line, f)) {
        char *eq = strchr(line, '=');
        if (!eq) continue;
        *eq = 0;
        std::string k(line), v(eq + 1);
        while (!v.empty() && (v.back() == '\n' || v.back() == '\r' || v.back() == ' ')) v.pop_back();
        if (k == "host") g_cfg.host = v;
        else if (k == "port") g_cfg.port = atoi(v.c_str());
        else if (k == "device") g_cfg.device = atoi(v.c_str());
        else if (k == "mirror") {
            auto c = v.find(':');
            if (c != std::string::npos) {
                g_cfg.mirrorHost = v.substr(0, c);
                g_cfg.mirrorPort = atoi(v.c_str() + c + 1);
            }
        }
        else if (k == "wifi_ssid") g_cfg.wifiSsid = v;
        else if (k == "wifi_pass") g_cfg.wifiPass = v;
        else if (k == "controllers") g_cfg.ctrlEnable = atoi(v.c_str()) != 0;
        else if (k == "left_device") g_cfg.ctrlLeftDevice = atoi(v.c_str());
        else if (k == "right_device") g_cfg.ctrlRightDevice = atoi(v.c_str());
        else if (k == "cam") g_cfg.camEnable = atoi(v.c_str()) != 0;
        else if (k == "cam_host") g_cfg.camHost = v;
        else if (k == "cam_port") g_cfg.camPort = atoi(v.c_str());
        else if (k == "cam_mjpeg") g_cfg.camMjpeg = atoi(v.c_str()) != 0;
        else if (k == "cam_w") g_cfg.camW = atoi(v.c_str());
        else if (k == "cam_h") g_cfg.camH = atoi(v.c_str());
        else if (k == "cam_fps") g_cfg.camFps = atoi(v.c_str());
    }
    fclose(f);
    if (g_cfg.camHost.empty()) g_cfg.camHost = g_cfg.host;
    LOGI("config: %s:%d device=%d mirror=%s:%d", g_cfg.host.c_str(), g_cfg.port,
         g_cfg.device, g_cfg.mirrorHost.c_str(), g_cfg.mirrorPort);
    if (g_cfg.camEnable)
        LOGI("camera: %s:%d %dx%d %s @%dfps", g_cfg.camHost.c_str(), g_cfg.camPort,
             g_cfg.camW, g_cfg.camH, g_cfg.camMjpeg ? "MJPEG" : "YUY2", g_cfg.camFps);
}

// ---- MPT1 (steamvr_driver/src/mapper_protocol.h): 68 bytes little-endian ----
int g_sock = -1;
sockaddr_in g_dst{}, g_mirror{};
bool g_haveMirror = false;

bool resolveTo(const std::string &host, int port, sockaddr_in *out)
{
    memset(out, 0, sizeof *out);
    out->sin_family = AF_INET;
    out->sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host.c_str(), &out->sin_addr) == 1) return true;
    addrinfo hints{}, *res = nullptr;
    hints.ai_family = AF_INET;
    if (getaddrinfo(host.c_str(), nullptr, &hints, &res) == 0 && res) {
        out->sin_addr = ((sockaddr_in *)res->ai_addr)->sin_addr;
        freeaddrinfo(res);
        return true;
    }
    return false;
}

void netInit()
{
    g_sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (!resolveTo(g_cfg.host, g_cfg.port, &g_dst))
        LOGE("cannot resolve %s", g_cfg.host.c_str());
    if (!g_cfg.mirrorHost.empty() && g_cfg.mirrorPort > 0)
        g_haveMirror = resolveTo(g_cfg.mirrorHost, g_cfg.mirrorPort, &g_mirror);
}

// ---- pose algebra (only what the recenter fix-up needs) ----
XrQuaternionf qMul(const XrQuaternionf &a, const XrQuaternionf &b)
{
    return {a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
            a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
            a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
            a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z};
}

XrVector3f qRot(const XrQuaternionf &q, const XrVector3f &v)
{
    // v + 2w(q x v) + 2(q x (q x v))
    const XrVector3f u{q.x, q.y, q.z};
    const XrVector3f uv{u.y * v.z - u.z * v.y, u.z * v.x - u.x * v.z,
                        u.x * v.y - u.y * v.x};
    const XrVector3f uuv{u.y * uv.z - u.z * uv.y, u.z * uv.x - u.x * uv.z,
                         u.x * uv.y - u.y * uv.x};
    return {v.x + 2.0f * (q.w * uv.x + uuv.x),
            v.y + 2.0f * (q.w * uv.y + uuv.y),
            v.z + 2.0f * (q.w * uv.z + uuv.z)};
}

// a then b, i.e. the pose b expressed through a.
XrPosef poseMul(const XrPosef &a, const XrPosef &b)
{
    XrPosef r;
    r.orientation = qMul(a.orientation, b.orientation);
    const XrVector3f rp = qRot(a.orientation, b.position);
    r.position = {a.position.x + rp.x, a.position.y + rp.y, a.position.z + rp.z};
    return r;
}

// Consecutive sendto() failures. ENETUNREACH/EHOSTUNREACH is exactly what a
// vanished route looks like, so this needs no permissions and no SSID read
// (which on modern Android would require location access).
std::atomic<int> g_sendFail{0};

// Battery per MPT1 slot, filled from Java (BatteryManager for the HMD,
// InputDevice.getBatteryState for the controllers). 0 = not reported.
std::atomic<unsigned> g_batt[3] = {{0}, {0}, {0}};   // (charging<<8) | pct

// ---- optional Bluetooth transport ----
// The BT socket lives in Java (android.bluetooth has no NDK equivalent), so the
// packet is handed up. Off unless bt=1, and it never replaces UDP -- both run,
// so the link can be compared side by side rather than swapped blind.
JavaVM *g_vm = nullptr;
jobject g_btObj = nullptr;       // global ref to the BtServer instance
jmethodID g_btSendMid = nullptr;
std::atomic<bool> g_btOn{false};

void btSend(const uint8_t *pkt68)
{
    if (!g_btOn.load() || !g_vm || !g_btObj || !g_btSendMid) return;
    JNIEnv *env = nullptr;
    // The pose loop is a native thread the VM has never seen; attaching once
    // per call would be absurd, so attach lazily and stay attached.
    if (g_vm->GetEnv((void **)&env, JNI_VERSION_1_6) != JNI_OK) {
        if (g_vm->AttachCurrentThread(&env, nullptr) != JNI_OK) return;
    }
    jbyteArray arr = env->NewByteArray(68);
    if (!arr) return;
    env->SetByteArrayRegion(arr, 0, 68, (const jbyte *)pkt68);
    env->CallVoidMethod(g_btObj, g_btSendMid, arr);
    if (env->ExceptionCheck()) env->ExceptionClear();
    env->DeleteLocalRef(arr);
}

// deviceId selects the MPT1 slot, so the HMD and each controller land on their
// own tracker in SteamVR.
void sendPose(int deviceId, const XrPosef &p, const XrVector3f &vel,
              const XrVector3f &ang, uint64_t tNs, bool valid)
{
    if (g_sock < 0) return;
    uint8_t m[68];
    const uint32_t magic = 0x3154504D;   // 'MPT1'
    const unsigned b = (deviceId >= 0 && deviceId < 3) ? g_batt[deviceId].load() : 0u;
    const uint8_t battPct = (uint8_t)(b & 0xFF);
    const uint8_t battFlags = (uint8_t)((b >> 8) & 1);
    const uint8_t dev = (uint8_t)deviceId, ok = valid ? 1 : 0;
    // MPT1 pose order is x,y,z,qw,qx,qy,qz; OpenXR quaternions are (x,y,z,w).
    // Both OpenXR and OpenVR are right-handed, y-up, -z-forward, so the axes
    // pass through unchanged -- only the quaternion component order differs.
    float pose[7] = {p.position.x, p.position.y, p.position.z,
                     p.orientation.w, p.orientation.x, p.orientation.y, p.orientation.z};
    float v[3] = {vel.x, vel.y, vel.z};
    float a[3] = {ang.x, ang.y, ang.z};
    memcpy(m + 0, &magic, 4); m[4] = dev; m[5] = ok;
    m[6] = battPct; m[7] = battFlags;
    memcpy(m + 8, &tNs, 8);
    memcpy(m + 16, pose, 28); memcpy(m + 44, v, 12); memcpy(m + 56, a, 12);
    if (sendto(g_sock, m, sizeof m, 0, (sockaddr *)&g_dst, sizeof g_dst) < 0) {
        if (errno == ENETUNREACH || errno == EHOSTUNREACH || errno == ENETDOWN)
            g_sendFail.fetch_add(1);
    } else {
        g_sendFail.store(0);
    }
    if (g_haveMirror)
        sendto(g_sock, m, sizeof m, 0, (sockaddr *)&g_mirror, sizeof g_mirror);
    btSend(m);
}

// ---- minimal EGL: OpenXR requires a graphics binding even to render nothing --
EGLDisplay g_egl = EGL_NO_DISPLAY;
EGLContext g_ctx = EGL_NO_CONTEXT;
EGLConfig  g_eglCfg = nullptr;

bool eglInit()
{
    g_egl = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (g_egl == EGL_NO_DISPLAY) return false;
    eglInitialize(g_egl, nullptr, nullptr);
    const EGLint attr[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
                           EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
                           EGL_NONE};
    EGLint n = 0;
    if (!eglChooseConfig(g_egl, attr, &g_eglCfg, 1, &n) || n < 1) return false;
    const EGLint ctxAttr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    g_ctx = eglCreateContext(g_egl, g_eglCfg, EGL_NO_CONTEXT, ctxAttr);
    if (g_ctx == EGL_NO_CONTEXT) return false;
    const EGLint pb[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLSurface s = eglCreatePbufferSurface(g_egl, g_eglCfg, pb);
    eglMakeCurrent(g_egl, s, s, g_ctx);
    return true;
}

// ---- OpenXR ----
XrInstance g_inst = XR_NULL_HANDLE;
XrSystemId g_sys = XR_NULL_SYSTEM_ID;
XrSession g_session = XR_NULL_HANDLE;
XrSpace g_local = XR_NULL_HANDLE, g_view = XR_NULL_HANDLE;
XrSessionState g_state = XR_SESSION_STATE_UNKNOWN;
bool g_running = false;

// Maps the CURRENT LOCAL space back into the LOCAL frame that existed when the
// session started -- the frame SpaceCalibrator solved against.
//
// A Quest that loses tracking and fails to relocalize RECENTERS: the LOCAL
// origin jumps to wherever the headset happens to be, and every calibration
// referencing the old origin is silently invalidated. The runtime announces
// this with XrEventDataReferenceSpaceChangePending, which carries the pose of
// the NEW space expressed in the OLD one -- exactly the correction needed to
// keep publishing in the original frame. Accumulating those keeps a calibration
// alive across any number of recenters.
XrPosef g_fix = {{0, 0, 0, 1}, {0, 0, 0}};
// Cleared if a recenter arrives with poseValid == false: the runtime is telling
// us the new origin has no known relation to the old, so the calibration is
// genuinely dead and poses must be published as invalid rather than as
// plausible-looking nonsense.
bool g_fixValid = true;

// ---- Touch controllers as trackers ----
// OpenXR exposes controller poses only through the action system, so this is
// the full action-set dance: action set -> pose action -> suggested bindings ->
// attach to session -> action spaces. Bound to the base Oculus Touch profile,
// which Quest Pro ("Pulsar") controllers also match.
//
// GRIP pose, not AIM: grip is the physical hold point and is what you want when
// the controller is strapped to a limb; aim is a pointing ray offset for UI.
XrActionSet g_actionSet = XR_NULL_HANDLE;
XrAction g_gripAction = XR_NULL_HANDLE;
XrPath g_handPath[2] = {XR_NULL_PATH, XR_NULL_PATH};
XrSpace g_handSpace[2] = {XR_NULL_HANDLE, XR_NULL_HANDLE};
bool g_ctrlReady = false;

// Converts CLOCK_MONOTONIC into XrTime. Null if the runtime lacks
// XR_KHR_convert_timespec_time, in which case sampling falls back to
// predictedDisplayTime -- see nowXrTime().
PFN_xrConvertTimespecTimeToTimeKHR g_toXrTime = nullptr;

bool extensionSupported(const char *name)
{
    uint32_t n = 0;
    if (XR_FAILED(xrEnumerateInstanceExtensionProperties(nullptr, 0, &n, nullptr)) || !n)
        return false;
    std::vector<XrExtensionProperties> props(n, {XR_TYPE_EXTENSION_PROPERTIES});
    if (XR_FAILED(xrEnumerateInstanceExtensionProperties(nullptr, n, &n, props.data())))
        return false;
    for (const auto &p : props)
        if (!strcmp(p.extensionName, name)) return true;
    return false;
}

// The instant the pose should be sampled at: NOW, not the compositor's next
// display time. A renderer wants predictedDisplayTime because its pose is
// consumed one frame later; a tracking puck does not -- the SteamVR driver
// stamps every sample with a NEGATIVE poseTimeOffset ("this is N seconds old")
// and extrapolates forward itself using the velocities. Sampling into the
// future and then declaring it a past sample double-counts the prediction, by
// an amount that varies with packet jitter -- which reads as microstutter.
// Returns 0 if the conversion is unavailable; caller falls back.
XrTime nowXrTime()
{
    if (!g_toXrTime || g_inst == XR_NULL_HANDLE) return 0;
    timespec ts{};
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    XrTime t = 0;
    if (XR_FAILED(g_toXrTime(g_inst, &ts, &t))) return 0;
    return t;
}

#define XR_OK(x, what) do { XrResult r__ = (x); if (XR_FAILED(r__)) { \
    LOGE("%s failed: %d", what, (int)r__); return false; } } while (0)

bool xrInit(android_app *app)
{
    PFN_xrInitializeLoaderKHR initLoader = nullptr;
    xrGetInstanceProcAddr(XR_NULL_HANDLE, "xrInitializeLoaderKHR",
                          (PFN_xrVoidFunction *)&initLoader);
    if (initLoader) {
        XrLoaderInitInfoAndroidKHR li{XR_TYPE_LOADER_INIT_INFO_ANDROID_KHR};
        li.applicationVM = app->activity->vm;
        li.applicationContext = app->activity->clazz;
        initLoader((const XrLoaderInitInfoBaseHeaderKHR *)&li);
    } else {
        LOGE("xrInitializeLoaderKHR unavailable -- no OpenXR runtime?");
        return false;
    }

    std::vector<const char *> exts = {XR_KHR_ANDROID_CREATE_INSTANCE_EXTENSION_NAME,
                                      XR_KHR_OPENGL_ES_ENABLE_EXTENSION_NAME};
    // Optional, and requested only if present: an unsupported extension name
    // fails xrCreateInstance outright, and we have a working fallback.
    const bool haveTimespec =
        extensionSupported(XR_KHR_CONVERT_TIMESPEC_TIME_EXTENSION_NAME);
    if (haveTimespec) exts.push_back(XR_KHR_CONVERT_TIMESPEC_TIME_EXTENSION_NAME);
    // Ask the runtime for low clocks ourselves. debug.oculus.cpuLevel/gpuLevel
    // do the same thing from adb but are volatile, so every reboot silently
    // restored full clocks on a device whose whole job is to sit on a belt
    // emitting poses. Measured on this workload: SUSTAINED_LOW costs nothing
    // (71.9 Hz, max inter-pose gap 14.6 ms), while the floor below it drops
    // poses outright (max gap 27.9 ms = one missed frame), so this does NOT ask
    // for POWER_SAVINGS.
    const bool havePerf = extensionSupported(XR_EXT_PERFORMANCE_SETTINGS_EXTENSION_NAME);
    if (havePerf) exts.push_back(XR_EXT_PERFORMANCE_SETTINGS_EXTENSION_NAME);

    // Quest Pro ("Pulsar") controllers bind to facebook/touch_controller_pro,
    // NOT the base oculus/touch_controller -- suggesting only the base profile
    // leaves them unbound and no controller poses arrive. Needs its extension
    // enabled before its bindings may be suggested.
    const bool haveProCtrl = extensionSupported("XR_FB_touch_controller_pro");
    if (haveProCtrl) exts.push_back("XR_FB_touch_controller_pro");

    XrInstanceCreateInfoAndroidKHR androidInfo{XR_TYPE_INSTANCE_CREATE_INFO_ANDROID_KHR};
    androidInfo.applicationVM = app->activity->vm;
    androidInfo.applicationActivity = app->activity->clazz;

    XrInstanceCreateInfo ici{XR_TYPE_INSTANCE_CREATE_INFO};
    ici.next = &androidInfo;
    ici.enabledExtensionCount = (uint32_t)exts.size();
    ici.enabledExtensionNames = exts.data();
    strcpy(ici.applicationInfo.applicationName, "MapperQuestTracker");
    // Pin OpenXR 1.0, do NOT use XR_CURRENT_API_VERSION: the Khronos loader AAR
    // ships 1.1 headers, and the Quest 1's frozen v50 runtime rejects a 1.1
    // request outright with XR_ERROR_API_VERSION_UNSUPPORTED (-4). Nothing here
    // needs 1.1 -- pose polling is 1.0 core.
    ici.applicationInfo.apiVersion = XR_MAKE_VERSION(1, 0, 34);
    XR_OK(xrCreateInstance(&ici, &g_inst), "xrCreateInstance");

    if (haveTimespec)
        xrGetInstanceProcAddr(g_inst, "xrConvertTimespecTimeToTimeKHR",
                              (PFN_xrVoidFunction *)&g_toXrTime);
    LOGI("pose sampling: %s", g_toXrTime ? "now (convert_timespec_time)"
                                         : "predictedDisplayTime (fallback)");

    XrSystemGetInfo sgi{XR_TYPE_SYSTEM_GET_INFO};
    sgi.formFactor = XR_FORM_FACTOR_HEAD_MOUNTED_DISPLAY;
    XR_OK(xrGetSystem(g_inst, &sgi, &g_sys), "xrGetSystem");

    // The runtime requires the GL binding be created against a context it has
    // vetted via xrGetOpenGLESGraphicsRequirementsKHR; skipping the query is a
    // spec violation some runtimes enforce.
    PFN_xrGetOpenGLESGraphicsRequirementsKHR getReq = nullptr;
    xrGetInstanceProcAddr(g_inst, "xrGetOpenGLESGraphicsRequirementsKHR",
                          (PFN_xrVoidFunction *)&getReq);
    XrGraphicsRequirementsOpenGLESKHR req{XR_TYPE_GRAPHICS_REQUIREMENTS_OPENGL_ES_KHR};
    if (getReq) getReq(g_inst, g_sys, &req);

    XrGraphicsBindingOpenGLESAndroidKHR gb{XR_TYPE_GRAPHICS_BINDING_OPENGL_ES_ANDROID_KHR};
    gb.display = g_egl;
    gb.config = g_eglCfg;
    gb.context = g_ctx;

    // Actions must be created on the INSTANCE and bindings suggested BEFORE the
    // session is attached; xrSuggestInteractionProfileBindings is illegal once
    // any session has action sets attached.
    if (g_cfg.ctrlEnable) {
        XrActionSetCreateInfo asci{XR_TYPE_ACTION_SET_CREATE_INFO};
        strcpy(asci.actionSetName, "tracker");
        strcpy(asci.localizedActionSetName, "Tracker");
        asci.priority = 0;
        if (XR_SUCCEEDED(xrCreateActionSet(g_inst, &asci, &g_actionSet))) {
            xrStringToPath(g_inst, "/user/hand/left", &g_handPath[0]);
            xrStringToPath(g_inst, "/user/hand/right", &g_handPath[1]);

            XrActionCreateInfo aci{XR_TYPE_ACTION_CREATE_INFO};
            aci.actionType = XR_ACTION_TYPE_POSE_INPUT;
            strcpy(aci.actionName, "grip_pose");
            strcpy(aci.localizedActionName, "Grip Pose");
            aci.countSubactionPaths = 2;
            aci.subactionPaths = g_handPath;
            if (XR_SUCCEEDED(xrCreateAction(g_actionSet, &aci, &g_gripAction))) {
                XrPath lGrip = XR_NULL_PATH, rGrip = XR_NULL_PATH;
                xrStringToPath(g_inst, "/user/hand/left/input/grip/pose", &lGrip);
                xrStringToPath(g_inst, "/user/hand/right/input/grip/pose", &rGrip);
                XrActionSuggestedBinding binds[2] = {{g_gripAction, lGrip},
                                                     {g_gripAction, rGrip}};
                // Suggest for every profile the controllers might bind to. The
                // runtime picks; suggesting extra profiles is harmless, but
                // omitting the one actually in use silently yields no poses.
                const char *profiles[2] = {
                    "/interaction_profiles/oculus/touch_controller",
                    haveProCtrl ? "/interaction_profiles/facebook/touch_controller_pro"
                                : nullptr};
                for (int pi = 0; pi < 2; pi++) {
                    if (!profiles[pi]) continue;
                    XrPath profile = XR_NULL_PATH;
                    if (XR_FAILED(xrStringToPath(g_inst, profiles[pi], &profile))) continue;
                    XrInteractionProfileSuggestedBinding sb{
                        XR_TYPE_INTERACTION_PROFILE_SUGGESTED_BINDING};
                    sb.interactionProfile = profile;
                    sb.countSuggestedBindings = 2;
                    sb.suggestedBindings = binds;
                    XrResult sr = xrSuggestInteractionProfileBindings(g_inst, &sb);
                    if (XR_FAILED(sr)) LOGE("suggest %s failed: %d", profiles[pi], (int)sr);
                    else { LOGI("bound profile %s", profiles[pi]); g_ctrlReady = true; }
                }
            }
        }
        if (!g_ctrlReady) LOGE("controller actions unavailable; HMD only");
    }

    XrSessionCreateInfo sci{XR_TYPE_SESSION_CREATE_INFO};
    sci.next = &gb;
    sci.systemId = g_sys;
    XR_OK(xrCreateSession(g_inst, &sci, &g_session), "xrCreateSession");

    if (havePerf) {
        PFN_xrPerfSettingsSetPerformanceLevelEXT setPerf = nullptr;
        xrGetInstanceProcAddr(g_inst, "xrPerfSettingsSetPerformanceLevelEXT",
                              (PFN_xrVoidFunction *)&setPerf);
        if (setPerf) {
            const XrPerfSettingsDomainEXT domains[2] = {
                XR_PERF_SETTINGS_DOMAIN_CPU_EXT, XR_PERF_SETTINGS_DOMAIN_GPU_EXT};
            for (int i = 0; i < 2; ++i) {
                XrResult r = setPerf(g_session, domains[i],
                                     XR_PERF_SETTINGS_LEVEL_SUSTAINED_LOW_EXT);
                if (XR_FAILED(r))
                    LOGI("perf level %s not accepted (%d)",
                         i == 0 ? "cpu" : "gpu", (int)r);
            }
            LOGI("requested SUSTAINED_LOW clocks (cpu+gpu)");
        }
    }

    if (g_ctrlReady) {
        XrSessionActionSetsAttachInfo ai{XR_TYPE_SESSION_ACTION_SETS_ATTACH_INFO};
        ai.countActionSets = 1;
        ai.actionSets = &g_actionSet;
        XrResult ar = xrAttachSessionActionSets(g_session, &ai);
        if (XR_FAILED(ar)) {
            LOGE("attach action sets failed: %d", (int)ar);
            g_ctrlReady = false;
        } else {
            for (int i = 0; i < 2; i++) {
                XrActionSpaceCreateInfo asp{XR_TYPE_ACTION_SPACE_CREATE_INFO};
                asp.action = g_gripAction;
                asp.subactionPath = g_handPath[i];
                asp.poseInActionSpace.orientation.w = 1.0f;
                if (XR_FAILED(xrCreateActionSpace(g_session, &asp, &g_handSpace[i]))) {
                    LOGE("action space %d failed", i);
                    g_ctrlReady = false;
                }
            }
            if (g_ctrlReady)
                LOGI("controllers enabled -> MPT1 devices %d (left) / %d (right)",
                     g_cfg.ctrlLeftDevice, g_cfg.ctrlRightDevice);
        }
    }

    XrReferenceSpaceCreateInfo rsci{XR_TYPE_REFERENCE_SPACE_CREATE_INFO};
    rsci.poseInReferenceSpace.orientation.w = 1.0f;
    rsci.referenceSpaceType = XR_REFERENCE_SPACE_TYPE_LOCAL;
    XR_OK(xrCreateReferenceSpace(g_session, &rsci, &g_local), "create LOCAL space");
    rsci.referenceSpaceType = XR_REFERENCE_SPACE_TYPE_VIEW;
    XR_OK(xrCreateReferenceSpace(g_session, &rsci, &g_view), "create VIEW space");

    LOGI("OpenXR ready");
    return true;
}

void pumpEvents()
{
    XrEventDataBuffer ev{XR_TYPE_EVENT_DATA_BUFFER};
    while (xrPollEvent(g_inst, &ev) == XR_SUCCESS) {
        if (ev.type == XR_TYPE_EVENT_DATA_REFERENCE_SPACE_CHANGE_PENDING) {
            auto *rc = (XrEventDataReferenceSpaceChangePending *)&ev;
            if (rc->referenceSpaceType == XR_REFERENCE_SPACE_TYPE_LOCAL) {
                if (rc->poseValid) {
                    // poseInPreviousSpace is the new origin expressed in the old
                    // one, so folding it in keeps g_fix mapping current -> the
                    // session-start frame however many times this fires.
                    g_fix = poseMul(g_fix, rc->poseInPreviousSpace);
                    const XrVector3f &t = rc->poseInPreviousSpace.position;
                    LOGI("LOCAL recentered by (%.3f %.3f %.3f) -- absorbed, "
                         "calibration preserved", t.x, t.y, t.z);
                } else {
                    g_fixValid = false;
                    LOGE("LOCAL recentered with NO known relation to the old "
                         "origin -- calibration is dead, publishing invalid");
                }
            }
        } else if (ev.type == XR_TYPE_EVENT_DATA_SESSION_STATE_CHANGED) {
            auto *ss = (XrEventDataSessionStateChanged *)&ev;
            g_state = ss->state;
            LOGI("session state -> %d", (int)g_state);
            if (g_state == XR_SESSION_STATE_READY) {
                XrSessionBeginInfo bi{XR_TYPE_SESSION_BEGIN_INFO};
                bi.primaryViewConfigurationType =
                    XR_VIEW_CONFIGURATION_TYPE_PRIMARY_STEREO;
                if (XR_SUCCEEDED(xrBeginSession(g_session, &bi))) g_running = true;
            } else if (g_state == XR_SESSION_STATE_STOPPING) {
                xrEndSession(g_session);
                g_running = false;
            }
        }
        ev = {XR_TYPE_EVENT_DATA_BUFFER};
    }
}

void frame()
{
    if (!g_running) return;
    XrFrameWaitInfo fwi{XR_TYPE_FRAME_WAIT_INFO};
    XrFrameState fs{XR_TYPE_FRAME_STATE};
    if (XR_FAILED(xrWaitFrame(g_session, &fwi, &fs))) return;
    XrFrameBeginInfo fbi{XR_TYPE_FRAME_BEGIN_INFO};
    xrBeginFrame(g_session, &fbi);

    // NOT gated on fs.shouldRender. That flag says whether the compositor wants
    // pixels, which for a zero-layer app the runtime is free to turn off -- but
    // this app's whole output is the pose, and skipping a sample punches a hole
    // in an otherwise steady stream.
    const XrTime nowT = nowXrTime();
    const XrTime sampleAt = nowT ? nowT : fs.predictedDisplayTime;
    XrSpaceVelocity vel{XR_TYPE_SPACE_VELOCITY};
    XrSpaceLocation loc{XR_TYPE_SPACE_LOCATION};
    loc.next = &vel;
    if (XR_SUCCEEDED(xrLocateSpace(g_view, g_local, sampleAt, &loc))) {
        // TRACKED, not merely VALID. During tracking loss the runtime keeps
        // reporting VALID poses that are inferred rather than observed -- those
        // drift, and publishing them as good is how a tracker slides quietly
        // away from the body instead of visibly dropping out.
        const bool posed =
            (loc.locationFlags & XR_SPACE_LOCATION_POSITION_VALID_BIT) &&
            (loc.locationFlags & XR_SPACE_LOCATION_ORIENTATION_VALID_BIT) &&
            (loc.locationFlags & XR_SPACE_LOCATION_POSITION_TRACKED_BIT) &&
            (loc.locationFlags & XR_SPACE_LOCATION_ORIENTATION_TRACKED_BIT) &&
            g_fixValid;
        XrVector3f lv{0, 0, 0}, av{0, 0, 0};
        if (vel.velocityFlags & XR_SPACE_VELOCITY_LINEAR_VALID_BIT) lv = vel.linearVelocity;
        if (vel.velocityFlags & XR_SPACE_VELOCITY_ANGULAR_VALID_BIT) av = vel.angularVelocity;
        // Re-express pose and velocities in the session-start frame. Velocities
        // are free vectors, so only g_fix's rotation applies to them.
        const XrPosef fixed = poseMul(g_fix, loc.pose);
        lv = qRot(g_fix.orientation, lv);
        av = qRot(g_fix.orientation, av);
        // XrTime is nanoseconds, and sampleAt is the instant the pose is FOR.
        sendPose(g_cfg.device, fixed, lv, av, (uint64_t)sampleAt, posed);
    }

    // ---- controllers ----
    // xrSyncActions only delivers poses while the session is FOCUSED; in any
    // other state it returns XR_SESSION_NOT_FOCUSED and the action spaces go
    // untracked, which surfaces below as valid=0 rather than a stale pose.
    if (g_ctrlReady) {
        XrActiveActionSet aas{g_actionSet, XR_NULL_PATH};
        XrActionsSyncInfo si{XR_TYPE_ACTIONS_SYNC_INFO};
        si.countActiveActionSets = 1;
        si.activeActionSets = &aas;
        xrSyncActions(g_session, &si);

        const int devIds[2] = {g_cfg.ctrlLeftDevice, g_cfg.ctrlRightDevice};
        for (int i = 0; i < 2; i++) {
            if (g_handSpace[i] == XR_NULL_HANDLE) continue;
            XrSpaceVelocity cvel{XR_TYPE_SPACE_VELOCITY};
            XrSpaceLocation cloc{XR_TYPE_SPACE_LOCATION};
            cloc.next = &cvel;
            if (XR_FAILED(xrLocateSpace(g_handSpace[i], g_local, sampleAt, &cloc)))
                continue;
            const XrSpaceLocationFlags f = cloc.locationFlags;
            const bool ok =
                (f & XR_SPACE_LOCATION_POSITION_VALID_BIT) &&
                (f & XR_SPACE_LOCATION_ORIENTATION_VALID_BIT) &&
                (f & XR_SPACE_LOCATION_POSITION_TRACKED_BIT) &&
                (f & XR_SPACE_LOCATION_ORIENTATION_TRACKED_BIT) && g_fixValid;
            XrVector3f clv{0, 0, 0}, cav{0, 0, 0};
            if (cvel.velocityFlags & XR_SPACE_VELOCITY_LINEAR_VALID_BIT)
                clv = cvel.linearVelocity;
            if (cvel.velocityFlags & XR_SPACE_VELOCITY_ANGULAR_VALID_BIT)
                cav = cvel.angularVelocity;
            // Same recenter fix-up as the HMD, so all three trackers stay in one
            // frame across a LOCAL origin jump.
            const XrPosef cfixed = poseMul(g_fix, cloc.pose);
            clv = qRot(g_fix.orientation, clv);
            cav = qRot(g_fix.orientation, cav);
            sendPose(devIds[i], cfixed, clv, cav, (uint64_t)sampleAt, ok);
        }
    }

    // Submit zero layers: legal, and it is what makes this a tracking puck
    // rather than a renderer.
    XrFrameEndInfo fei{XR_TYPE_FRAME_END_INFO};
    fei.displayTime = fs.predictedDisplayTime;
    fei.environmentBlendMode = XR_ENVIRONMENT_BLEND_MODE_OPAQUE;
    fei.layerCount = 0;
    fei.layers = nullptr;
    xrEndFrame(g_session, &fei);
}

#if Q1T_UVC
// ---------------------------------------------------------------------------
// Known-pose mapping camera
//
// A UVC module bolted to the Quest. Each frame is tagged, ON DEVICE, with the
// headset pose at the frame's own capture instant and streamed as MPF1. Pairing
// here rather than at the host is the entire point: the pose comes from the same
// clock as the image, so the map is triangulated from poses that actually match
// the frames (CLAUDE.md §2.1), not from whatever pose happened to be current
// when a jittery Wi-Fi packet landed.
// ---------------------------------------------------------------------------
int g_camSock = -1;
sockaddr_in g_camDst{};
struct uvc_state g_uvc{};        // lives as long as the capture thread
std::thread g_camSender, g_camCapture;
std::atomic<int> g_camFd{-1};
std::atomic<bool> g_camRun{false};

// CLOCK_BOOTTIME - CLOCK_MONOTONIC, in ns. UVC stamps frames in BOOTTIME;
// xrConvertTimespecTimeToTimeKHR wants MONOTONIC. The two differ only by time
// spent suspended, which for a live tracking session is a fixed constant, so
// sampling the offset once is enough.
int64_t g_bootMinusMonoNs = 0;

// mailbox: newest posed frame awaiting the sender. Single slot -- under network
// backpressure a stale frame is worth dropping, and mapping tolerates gaps.
std::mutex g_camMx;
std::condition_variable g_camCv;
std::vector<uint8_t> g_camMsg;   // full MPF1 record (header + jpeg)
bool g_camReady = false;
uint64_t g_camIndex = 0, g_camDropped = 0, g_camSent = 0;

void initCamClock()
{
    timespec tb{}, tm{};
    clock_gettime(CLOCK_BOOTTIME, &tb);
    clock_gettime(CLOCK_MONOTONIC, &tm);
    int64_t boot = (int64_t)tb.tv_sec * 1000000000LL + tb.tv_nsec;
    int64_t mono = (int64_t)tm.tv_sec * 1000000000LL + tm.tv_nsec;
    g_bootMinusMonoNs = boot - mono;
}

XrTime bootToXrTime(uint64_t bootNs)
{
    if (!g_toXrTime) return nowXrTime();   // no converter: best-effort (adds latency)
    int64_t monoNs = (int64_t)bootNs - g_bootMinusMonoNs;
    if (monoNs < 0) return nowXrTime();
    timespec ts{};
    ts.tv_sec = (time_t)(monoNs / 1000000000LL);
    ts.tv_nsec = (long)(monoNs % 1000000000LL);
    XrTime t = 0;
    if (XR_FAILED(g_toXrTime(g_inst, &ts, &t))) return nowXrTime();
    return t;
}

// UVC callback (USB event thread). Locate the headset pose at the frame's
// capture time, pack MPF1, hand off to the sender. Kept short: no network here.
void onCamFrame(void * /*user*/, const uint8_t *data, size_t len, uint64_t tsNs,
                int /*index*/)
{
    uint8_t poseValid = 0;
    float pose[7] = {0, 0, 0, 1, 0, 0, 0};
    if (g_view != XR_NULL_HANDLE && g_local != XR_NULL_HANDLE) {
        XrTime xt = bootToXrTime(tsNs);
        XrSpaceLocation loc{XR_TYPE_SPACE_LOCATION};
        if (xt && XR_SUCCEEDED(xrLocateSpace(g_view, g_local, xt, &loc))) {
            const XrSpaceLocationFlags f = loc.locationFlags;
            const bool tracked =
                (f & XR_SPACE_LOCATION_POSITION_VALID_BIT) &&
                (f & XR_SPACE_LOCATION_ORIENTATION_VALID_BIT) &&
                (f & XR_SPACE_LOCATION_POSITION_TRACKED_BIT) &&
                (f & XR_SPACE_LOCATION_ORIENTATION_TRACKED_BIT) && g_fixValid;
            if (tracked) {
                // Same frame the MPT1 poses use: LOCAL with recenters folded in.
                const XrPosef p = poseMul(g_fix, loc.pose);
                pose[0] = p.position.x; pose[1] = p.position.y; pose[2] = p.position.z;
                pose[3] = p.orientation.w; pose[4] = p.orientation.x;
                pose[5] = p.orientation.y; pose[6] = p.orientation.z;
                poseValid = 1;
            }
        }
    }

    PosedFrameHeader h{};
    h.magic = kPosedFrameMagic;
    h.t_ns = tsNs;
    h.index = (uint32_t)g_camIndex++;
    h.pose_valid = poseValid;
    memcpy(h.pose, pose, sizeof pose);
    h.len = (uint32_t)len;

    std::vector<uint8_t> msg(sizeof h + len);
    memcpy(msg.data(), &h, sizeof h);
    memcpy(msg.data() + sizeof h, data, len);

    {
        std::lock_guard<std::mutex> lk(g_camMx);
        if (g_camReady) g_camDropped++;      // overwrite the unsent one
        g_camMsg.swap(msg);
        g_camReady = true;
    }
    g_camCv.notify_one();
}

bool camConnect()
{
    g_camSock = socket(AF_INET, SOCK_STREAM, 0);
    if (g_camSock < 0) return false;
    if (connect(g_camSock, (sockaddr *)&g_camDst, sizeof g_camDst) != 0) {
        close(g_camSock);
        g_camSock = -1;
        return false;
    }
    LOGI("camera: connected to %s:%d", g_cfg.camHost.c_str(), g_cfg.camPort);
    return true;
}

void camSenderLoop()
{
    if (!resolveTo(g_cfg.camHost, g_cfg.camPort, &g_camDst)) {
        LOGE("camera: cannot resolve %s", g_cfg.camHost.c_str());
        return;
    }
    std::vector<uint8_t> out;
    while (g_camRun.load()) {
        if (g_camSock < 0) {
            if (!camConnect()) {
                std::this_thread::sleep_for(std::chrono::seconds(2));
                continue;
            }
        }
        {
            std::unique_lock<std::mutex> lk(g_camMx);
            g_camCv.wait_for(lk, std::chrono::seconds(1),
                             [] { return g_camReady || !g_camRun.load(); });
            if (!g_camReady) continue;       // timeout or shutdown; re-check state
            out.swap(g_camMsg);
            g_camReady = false;
        }
        size_t off = 0;
        bool ok = true;
        while (off < out.size()) {
            ssize_t n = send(g_camSock, out.data() + off, out.size() - off, MSG_NOSIGNAL);
            if (n <= 0) { ok = false; break; }
            off += (size_t)n;
        }
        if (!ok) {
            LOGE("camera: send failed, will reconnect");
            close(g_camSock);
            g_camSock = -1;
        } else {
            g_camSent++;
        }
    }
    if (g_camSock >= 0) { close(g_camSock); g_camSock = -1; }
}

void camCaptureLoop(int fd)
{
    g_uvc = {};
    g_uvc.fd = fd;
    g_uvc.is_mjpeg = g_cfg.camMjpeg ? 1 : 0;
    g_uvc.fmt_index = g_cfg.camMjpeg ? 1 : 2;
    g_uvc.frame_index = 1;
    g_uvc.width = g_cfg.camW;
    g_uvc.height = g_cfg.camH;
    // Frame interval in 100 ns units, from the requested fps.
    g_uvc.interval = g_cfg.camFps > 0 ? (uint32_t)(10000000 / g_cfg.camFps) : 333333;
    g_uvc.frames_wanted = 0;                 // until stopped
    g_uvc.expect_bytes = (size_t)g_cfg.camW * g_cfg.camH * 2;   // YUY2 tear check
    g_uvc.cb = onCamFrame;
    g_uvc.cb_user = nullptr;
    LOGI("camera: capture starting (fd=%d)", fd);
    int rc = uvc_run(&g_uvc);
    LOGI("camera: capture stopped rc=%d (%s) frames=%d torn=%d dropped=%d sent=%llu",
         rc, g_uvc.err, g_uvc.frames_done, g_uvc.frames_torn, g_uvc.frames_dropped,
         (unsigned long long)g_camSent);
}

// Spawn the sender once at startup; capture waits for a USB fd from Java.
void cameraStart()
{
    if (!g_cfg.camEnable) return;
    initCamClock();
    g_camRun.store(true);
    g_camSender = std::thread(camSenderLoop);
    LOGI("camera: enabled, waiting for USB fd from Java");
}

// Called from the JNI thread when Java has opened the UVC device.
void cameraSetFd(int fd)
{
    if (!g_cfg.camEnable) { LOGE("camera: fd arrived but cam=0 in config"); return; }
    if (g_camCapture.joinable()) {
        LOGI("camera: replacing existing capture");
        g_uvc.stop = 1;
        g_camCapture.join();
    }
    g_camFd.store(fd);
    g_camCapture = std::thread(camCaptureLoop, fd);
}

void cameraStop()
{
    g_camRun.store(false);
    g_uvc.stop = 1;
    g_camCv.notify_all();
    if (g_camCapture.joinable()) g_camCapture.join();
    if (g_camSender.joinable()) g_camSender.join();
}
#else
// UVC (external mapping camera) is compiled out. Q2Slam reads the Quest's own
// tracking cameras from a rooted native process instead, so this path is dead
// weight here -- and it was never run against real hardware upstream. The
// entry points remain as no-ops so the JNI symbol still resolves.
void cameraStart() {}
void cameraSetFd(int) {}
void cameraStop() {}
#endif  // Q1T_UVC

}  // namespace

extern "C" void android_main(android_app *app)
{
    app->onAppCmd = [](android_app *, int32_t) {};
    loadConfig(app->activity->externalDataPath);
    netInit();

    if (!eglInit()) { LOGE("EGL init failed"); return; }
    if (!xrInit(app)) { LOGE("OpenXR init failed"); return; }

    cameraStart();   // no-op unless cam=1; capture waits for a fd from Java

    while (!app->destroyRequested) {
        int events;
        android_poll_source *src;
        while (ALooper_pollOnce(g_running ? 0 : 100, nullptr, &events,
                                (void **)&src) >= 0) {
            if (src) src->process(app, src);
            if (app->destroyRequested) break;
        }
        pumpEvents();
        frame();
    }
    cameraStop();
    LOGI("exiting");
}

// Java (MainActivity) opens the UVC device -- the platform only grants USB access
// through the Java UsbManager -- and hands the raw fd here. libusb wraps it with
// no enumeration or root (see uvc_android.c). fd < 0 means the camera detached.
extern "C" JNIEXPORT void JNICALL
Java_com_mapperlocalizer_questtracker_MainActivity_nativeSetCameraFd(
    JNIEnv * /*env*/, jobject /*thiz*/, jint fd)
{
    if (fd < 0) cameraStop();
    else        cameraSetFd((int)fd);
}

// Hand the BtServer instance down so the pose path can push packets into it.
// obj == null tears the link down.
extern "C" JNIEXPORT void JNICALL
Java_com_mapperlocalizer_questtracker_MainActivity_nativeSetBtServer(
    JNIEnv *env, jobject /*thiz*/, jobject btServer)
{
    if (g_btObj) { env->DeleteGlobalRef(g_btObj); g_btObj = nullptr; }
    g_btSendMid = nullptr;
    g_btOn.store(false);
    if (!btServer) { LOGI("BT transport off"); return; }
    env->GetJavaVM(&g_vm);
    g_btObj = env->NewGlobalRef(btServer);
    jclass cls = env->GetObjectClass(btServer);
    g_btSendMid = env->GetMethodID(cls, "send", "([B)V");
    if (!g_btSendMid) {
        LOGE("BT: send([B)V not found");
        if (env->ExceptionCheck()) env->ExceptionClear();
        return;
    }
    g_btOn.store(true);
    LOGI("BT transport on (poses also still go over UDP)");
}

// How many consecutive poses failed to leave the device because there is no
// route. Java polls this to decide when to ask for the tracker network back.
extern "C" JNIEXPORT jint JNICALL
Java_com_mapperlocalizer_questtracker_MainActivity_nativeSendFailures(
    JNIEnv * /*env*/, jobject /*thiz*/)
{
    return (jint)g_sendFail.load();
}

// The SSID/password the tracker belongs on, from config.txt, so Java does not
// need its own parser.
extern "C" JNIEXPORT jstring JNICALL
Java_com_mapperlocalizer_questtracker_MainActivity_nativeWifiSsid(
    JNIEnv *env, jobject /*thiz*/)
{
    return env->NewStringUTF(g_cfg.wifiSsid.c_str());
}

extern "C" JNIEXPORT jstring JNICALL
Java_com_mapperlocalizer_questtracker_MainActivity_nativeWifiPass(
    JNIEnv *env, jobject /*thiz*/)
{
    return env->NewStringUTF(g_cfg.wifiPass.c_str());
}

// Battery for one MPT1 slot. Only Java can read BatteryManager and
// InputDevice.getBatteryState, so the values are pushed down rather than polled
// here. pct 0 = unknown.
extern "C" JNIEXPORT void JNICALL
Java_com_mapperlocalizer_questtracker_MainActivity_nativeSetBattery(
    JNIEnv * /*env*/, jobject /*thiz*/, jint device, jint pct, jboolean charging)
{
    if (device < 0 || device > 2) return;
    if (pct < 0) pct = 0;
    if (pct > 100) pct = 100;
    g_batt[device].store(((charging ? 1u : 0u) << 8) | (unsigned)pct);
}
