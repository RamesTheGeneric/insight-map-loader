// driver_mapper.cpp -- SteamVR driver publishing the body trackers.
//
// Architecture (CLAUDE.md §0, per-tracker mono-inertial):
//   each tracker computes its filtered pose on-device and streams it here over
//   UDP; this driver relays those poses to SteamVR as TrackedDeviceClass_
//   GenericTracker devices with waist / left-foot / right-foot role hints.
//
// The driver deliberately does no estimation. Its only jobs are:
//   * stand up N generic trackers and pre-assign their FBT roles,
//   * receive pose packets (mapper_protocol.h) on a background thread,
//   * hand SteamVR each latest pose with velocity + a negative poseTimeOffset
//     so the compositor extrapolates through the remaining network/render
//     latency (buying back the on-device SLAM latency, per the pivot plan).
//
// It links nothing from OpenVR: a driver is a DLL exporting HmdDriverFactory
// and receives every interface it needs through the context handed to Init().

#include <openvr_driver.h>

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>

#include "mapper_protocol.h"

#if defined(_WIN32)
#include <winsock2.h>
#include <ws2tcpip.h>
#pragma comment(lib, "ws2_32.lib")
using socket_t = SOCKET;
static const socket_t kBadSocket = INVALID_SOCKET;
#else
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>
using socket_t = int;
static const socket_t kBadSocket = -1;
#endif

using namespace vr;

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------
namespace {

std::string g_logPrefix = "[mapper] ";

void Log(const std::string& msg) {
    if (VRDriverLog())
        VRDriverLog()->Log((g_logPrefix + msg).c_str());
}

double NowSeconds() {  // monotonic, driver-local clock
    using namespace std::chrono;
    return duration<double>(steady_clock::now().time_since_epoch()).count();
}

HmdQuaternion_t QuatIdentity() { return {1.0, 0.0, 0.0, 0.0}; }

// Per-device static configuration.
struct DeviceCfg {
    const char* serial;        // stable, unique -> SteamVR remembers pairings
    const char* controllerType;// carries the FBT role to the input system
    const char* trackerRole;   // steamvr.vrsettings "trackers" role string
    const char* model;
};

// APPEND ONLY -- see the note on MapperDeviceId. Renaming a serial makes
// SteamVR forget that tracker's pairing, role and calibration.
const DeviceCfg kDevices[MAPPER_DEV_COUNT] = {
    {"mapper_waist",          "vive_tracker_waist",          "TrackerRole_Waist",         "Mapper Waist"},
    {"mapper_left_foot",      "vive_tracker_left_foot",      "TrackerRole_LeftFoot",      "Mapper Left Foot"},
    {"mapper_right_foot",     "vive_tracker_right_foot",     "TrackerRole_RightFoot",     "Mapper Right Foot"},
    {"mapper_chest",          "vive_tracker_chest",          "TrackerRole_Chest",         "Mapper Chest"},
    {"mapper_left_knee",      "vive_tracker_left_knee",      "TrackerRole_LeftKnee",      "Mapper Left Knee"},
    {"mapper_right_knee",     "vive_tracker_right_knee",     "TrackerRole_RightKnee",     "Mapper Right Knee"},
    {"mapper_left_elbow",     "vive_tracker_left_elbow",     "TrackerRole_LeftElbow",     "Mapper Left Elbow"},
    {"mapper_right_elbow",    "vive_tracker_right_elbow",    "TrackerRole_RightElbow",    "Mapper Right Elbow"},
    {"mapper_left_shoulder",  "vive_tracker_left_shoulder",  "TrackerRole_LeftShoulder",  "Mapper Left Shoulder"},
    {"mapper_right_shoulder", "vive_tracker_right_shoulder", "TrackerRole_RightShoulder", "Mapper Right Shoulder"},
    {"mapper_camera",         "vive_tracker_camera",         "TrackerRole_Camera",        "Mapper Camera"},
};
static_assert(sizeof(kDevices) / sizeof(kDevices[0]) == MAPPER_DEV_COUNT,
              "kDevices must have exactly MAPPER_DEV_COUNT entries");

}  // namespace

// ---------------------------------------------------------------------------
// one tracker device
// ---------------------------------------------------------------------------
class MapperTracker : public ITrackedDeviceServerDriver {
public:
    explicit MapperTracker(int devId) : m_devId(devId) {
        m_pose = {};
        m_pose.poseIsValid = false;
        // NOT connected until a packet for this device id actually arrives.
        // All three trackers are registered up front so their FBT roles can be
        // pre-assigned, but reporting an unfed tracker as connected makes FBT
        // solvers treat it as a real, present tracker: with a role bound and no
        // pose of its own, the avatar's IK drags it around off the tracker that
        // IS live, which looks like a phantom limb following the waist.
        m_pose.deviceIsConnected = false;
        m_pose.result = TrackingResult_Uninitialized;
        m_pose.qWorldFromDriverRotation = QuatIdentity();
        m_pose.qDriverFromHeadRotation = QuatIdentity();
        m_pose.qRotation = QuatIdentity();
        // The tracker's origin IS the reported point; no head offset.
        m_pose.vecDriverFromHeadTranslation[0] = 0.0;
        m_pose.vecDriverFromHeadTranslation[1] = 0.0;
        m_pose.vecDriverFromHeadTranslation[2] = 0.0;
    }

    // --- ITrackedDeviceServerDriver ---
    EVRInitError Activate(TrackedDeviceIndex_t objectId) override {
        m_objectId = objectId;
        PropertyContainerHandle_t c =
            VRProperties()->TrackedDeviceToPropertyContainer(objectId);

        const DeviceCfg& d = kDevices[m_devId];
        VRProperties()->SetStringProperty(c, Prop_TrackingSystemName_String, "mapper");
        VRProperties()->SetStringProperty(c, Prop_ManufacturerName_String, "Mapper-Localizer");
        VRProperties()->SetStringProperty(c, Prop_ModelNumber_String, d.model);
        VRProperties()->SetStringProperty(c, Prop_SerialNumber_String, d.serial);
        // Render + input-bind as a Vive tracker so existing apps (VRChat FBT)
        // pick up the role with no per-app config.
        VRProperties()->SetStringProperty(c, Prop_RenderModelName_String,
                                          "{htc}vr_tracker_vive_1_0");
        VRProperties()->SetStringProperty(c, Prop_InputProfilePath_String,
                                          "{htc}/input/vive_tracker_profile.json");
        VRProperties()->SetStringProperty(c, Prop_ControllerType_String, d.controllerType);
        VRProperties()->SetInt32Property(c, Prop_DeviceClass_Int32,
                                         TrackedDeviceClass_GenericTracker);
        VRProperties()->SetBoolProperty(c, Prop_WillDriftInYaw_Bool, false);
        VRProperties()->SetBoolProperty(c, Prop_DeviceIsWireless_Bool, true);
        VRProperties()->SetBoolProperty(c, Prop_DeviceIsCharging_Bool, false);
        VRProperties()->SetBoolProperty(c, Prop_Identifiable_Bool, false);
        VRProperties()->SetBoolProperty(c, Prop_DeviceProvidesBatteryStatus_Bool, false);
        VRProperties()->SetInt32Property(c, Prop_ControllerRoleHint_Int32,
                                         TrackedControllerRole_Invalid);

        // Pre-assign the FBT role so the user doesn't have to open "Manage
        // Trackers". Key is the device path SteamVR uses in steamvr.vrsettings.
        std::string devPath = std::string("/devices/mapper/") + d.serial;
        VRSettings()->SetString(k_pch_Trackers_Section, devPath.c_str(), d.trackerRole);

        Log(std::string("activated ") + d.serial + " as " + d.trackerRole);
        return VRInitError_None;
    }

    void Deactivate() override { m_objectId = k_unTrackedDeviceIndexInvalid; }
    void EnterStandby() override {}
    void* GetComponent(const char*) override { return nullptr; }
    void DebugRequest(const char*, char* buf, uint32_t n) override {
        if (buf && n) buf[0] = '\0';
    }

    DriverPose_t GetPose() override {
        std::lock_guard<std::mutex> lk(m_mtx);
        return m_pose;
    }

    // Estimate producer->host clock offset from a packet's own timestamp.
    //
    // Ages must come from the instant the pose is FOR, not from when the packet
    // happened to land: arrival time carries the transport's jitter straight
    // into poseTimeOffset, and SteamVR then extrapolates by a wobbling amount,
    // which is visible as microstutter. The MINIMUM of (arrival - t_ns) over a
    // window is the robust estimator here, because network delay is one-sided
    // -- packets can be late but never early, so the least-delayed sample is
    // the closest thing to the true offset. A sliding window lets it track slow
    // clock drift. Returns the host-clock instant the pose corresponds to.
    double HostTimeForPacket(uint64_t t_ns, double arrival) {
        if (t_ns == 0) return arrival;            // producer sent no timestamp
        const double dev = (double)t_ns * 1e-9;
        const double d = arrival - dev;
        if (m_offN < kOffWindow) m_offBuf[m_offN++] = d;
        else { m_offBuf[m_offI] = d; m_offI = (m_offI + 1) % kOffWindow; }
        double mn = m_offBuf[0];
        for (int i = 1; i < m_offN; i++) if (m_offBuf[i] < mn) mn = m_offBuf[i];
        m_clockOffset = mn;
        const double host = dev + m_clockOffset;
        // A pose stamped in the future is nonsense; clamp so a bad clock cannot
        // make poseTimeOffset positive and send SteamVR extrapolating backwards.
        return host > arrival ? arrival : host;
    }

    // Called from the network thread when a fresh packet arrives.
    void OnPacket(const MapperPosePacket& p) {
        DriverPose_t pose = {};
        pose.qWorldFromDriverRotation = QuatIdentity();
        pose.qDriverFromHeadRotation = QuatIdentity();
        pose.deviceIsConnected = true;

        pose.vecPosition[0] = p.pose[0];
        pose.vecPosition[1] = p.pose[1];
        pose.vecPosition[2] = p.pose[2];
        pose.qRotation.w = p.pose[3];
        pose.qRotation.x = p.pose[4];
        pose.qRotation.y = p.pose[5];
        pose.qRotation.z = p.pose[6];

        pose.vecVelocity[0] = p.vel[0];
        pose.vecVelocity[1] = p.vel[1];
        pose.vecVelocity[2] = p.vel[2];
        pose.vecAngularVelocity[0] = p.angvel[0];
        pose.vecAngularVelocity[1] = p.angvel[1];
        pose.vecAngularVelocity[2] = p.angvel[2];

        const bool ok = p.valid != 0;
        pose.poseIsValid = ok;
        pose.result = ok ? TrackingResult_Running_OK : TrackingResult_Running_OutOfRange;

        const double arrival = NowSeconds();
        DriverPose_t out;
        {
            std::lock_guard<std::mutex> lk(m_mtx);
            // Age the pose from its own timestamp, and stamp poseTimeOffset HERE
            // rather than leaving it 0 until the next Tick. Publishing offset=0
            // on arrival and -(age) a moment later made SteamVR alternate
            // between not extrapolating and extrapolating, every single packet.
            m_poseHostTime = HostTimeForPacket(p.t_ns, arrival);
            m_lastRecv = arrival;
            pose.poseTimeOffset = -((arrival - m_poseHostTime) + m_extraLatency);
            m_pose = pose;
            m_havePose = true;
            m_battPct = p.battery_pct;
            m_battCharging = (p.battery_flags & 1) != 0;
            out = m_pose;
        }
        if (m_objectId != k_unTrackedDeviceIndexInvalid)
            VRServerDriverHost()->TrackedDevicePoseUpdated(m_objectId, out,
                                                           sizeof(DriverPose_t));
        PublishBattery();
    }

    // Battery is a property, not part of DriverPose_t, so it is pushed
    // separately and only when it actually changes -- property writes are not
    // free and the value moves once a minute at most.
    void PublishBattery() {
        if (m_objectId == k_unTrackedDeviceIndexInvalid) return;
        uint8_t pct; bool chg;
        {
            std::lock_guard<std::mutex> lk(m_mtx);
            pct = m_battPct; chg = m_battCharging;
        }
        if (pct == m_battPublished && chg == m_chgPublished) return;
        m_battPublished = pct;
        m_chgPublished = chg;
        PropertyContainerHandle_t c =
            VRProperties()->TrackedDeviceToPropertyContainer(m_objectId);
        // 0 means "not reported" -- say so rather than claiming a flat battery.
        VRProperties()->SetBoolProperty(c, Prop_DeviceProvidesBatteryStatus_Bool,
                                        pct != 0);
        if (pct != 0)
            VRProperties()->SetFloatProperty(c, Prop_DeviceBatteryPercentage_Float,
                                             (float)pct / 100.0f);
        VRProperties()->SetBoolProperty(c, Prop_DeviceIsCharging_Bool, chg);
    }

    // Called every RunFrame: refresh poseTimeOffset (pose ages between packets)
    // and drop to "not tracking" if the producer went silent.
    void Tick(double staleTimeout, double extraLatency) {
        std::lock_guard<std::mutex> lk(m_mtx);
        if (!m_havePose) {
            // Still publish, so SteamVR is told this tracker is disconnected
            // rather than being left to infer it from a GetPose() poll.
            if (m_objectId != k_unTrackedDeviceIndexInvalid)
                VRServerDriverHost()->TrackedDevicePoseUpdated(m_objectId, m_pose,
                                                               sizeof(DriverPose_t));
            return;
        }
        m_extraLatency = extraLatency;   // so OnPacket stamps the same way
        const double now = NowSeconds();
        if (now - m_lastRecv > staleTimeout) {
            m_pose.poseIsValid = false;
            m_pose.result = TrackingResult_Running_OutOfRange;
        }
        // Age from the instant the pose is FOR, matching OnPacket exactly, so
        // the offset only ever grows smoothly between packets instead of
        // jumping each time one lands.
        const double age = now - m_poseHostTime;
        // Negative offset = "this sample is (age + pipeline) seconds old";
        // SteamVR extrapolates forward to photon time using the velocities.
        m_pose.poseTimeOffset = -(age + extraLatency);
        if (m_objectId != k_unTrackedDeviceIndexInvalid)
            VRServerDriverHost()->TrackedDevicePoseUpdated(m_objectId, m_pose,
                                                           sizeof(DriverPose_t));
    }

    const char* Serial() const { return kDevices[m_devId].serial; }

private:
    int m_devId;
    TrackedDeviceIndex_t m_objectId = k_unTrackedDeviceIndexInvalid;
    std::mutex m_mtx;
    DriverPose_t m_pose{};
    double m_lastRecv = 0.0;        // arrival, for the stale-timeout only
    double m_poseHostTime = 0.0;    // host-clock instant the pose is FOR
    double m_extraLatency = 0.0;
    bool m_havePose = false;

    // producer->host clock offset, min-filtered (see HostTimeForPacket)
    static const int kOffWindow = 512;   // ~7 s at 72 Hz
    double m_offBuf[kOffWindow]{};
    int m_offN = 0, m_offI = 0;
    double m_clockOffset = 0.0;

    uint8_t m_battPct = 0;
    bool m_battCharging = false;
    uint8_t m_battPublished = 0xFF;   // force first publish
    bool m_chgPublished = false;
};

// ---------------------------------------------------------------------------
// provider: owns the trackers + the UDP receive thread
// ---------------------------------------------------------------------------
class MapperProvider : public IServerTrackedDeviceProvider {
public:
    EVRInitError Init(IVRDriverContext* ctx) override {
        VR_INIT_SERVER_DRIVER_CONTEXT(ctx);

        m_port = (int)VRSettings()->GetInt32("driver_mapper", "udp_port");
        if (m_port <= 0) m_port = 5180;  // default; also stated in default.vrsettings
        m_staleTimeout = VRSettings()->GetFloat("driver_mapper", "stale_timeout_s");
        if (m_staleTimeout <= 0.0) m_staleTimeout = 0.5;
        m_extraLatency = VRSettings()->GetFloat("driver_mapper", "pipeline_latency_s");
        if (m_extraLatency < 0.0) m_extraLatency = 0.025;

        // Construct every role, but do NOT announce them yet. Announcing all
        // of them would put a dozen ghost trackers in SteamVR for a fleet of
        // three pucks. A role appears the first time a packet for it arrives
        // (see RunFrame), which is also the first moment it can have a pose.
        for (int i = 0; i < MAPPER_DEV_COUNT; ++i) {
            m_trackers[i] = new MapperTracker(i);
            // Bind the FBT role up front even though the device does not exist
            // yet: the trackers section is a role map keyed by device path, so
            // writing it early costs nothing and means the role is already
            // correct the instant the tracker does appear.
            std::string devPath = std::string("/devices/mapper/") + kDevices[i].serial;
            VRSettings()->SetString(k_pch_Trackers_Section, devPath.c_str(),
                                    kDevices[i].trackerRole);
        }

        if (!StartSocket()) {
            Log("FATAL: could not open UDP socket; no poses will flow");
            return VRInitError_Driver_Failed;
        }
        m_run = true;
        m_thread = std::thread(&MapperProvider::RecvLoop, this);
        Log("init ok, listening on udp/" + std::to_string(m_port));
        return VRInitError_None;
    }

    void Cleanup() override {
        m_run = false;
        if (m_sock != kBadSocket) {
#if defined(_WIN32)
            closesocket(m_sock);
            WSACleanup();
#else
            close(m_sock);
#endif
            m_sock = kBadSocket;
        }
        if (m_thread.joinable()) m_thread.join();
        for (auto*& t : m_trackers) { delete t; t = nullptr; }
        VR_CLEANUP_SERVER_DRIVER_CONTEXT();
    }

    const char* const* GetInterfaceVersions() override {
        return k_InterfaceVersions;
    }

    void RunFrame() override {
        // Announce roles that have started receiving packets. This happens
        // HERE, not in RecvLoop: TrackedDeviceAdded synchronously creates the
        // property container and calls Activate on the calling thread, and
        // RunFrame is the server's main thread, which is where OpenVR expects
        // that. Doing it from the recv thread races SteamVR's own frame.
        for (int i = 0; i < MAPPER_DEV_COUNT; ++i) {
            if (m_seen[i].load(std::memory_order_relaxed) && !m_added[i]) {
                m_added[i] = true;
                VRServerDriverHost()->TrackedDeviceAdded(
                    m_trackers[i]->Serial(), TrackedDeviceClass_GenericTracker,
                    m_trackers[i]);
                Log(std::string("role appeared: ") + kDevices[i].serial);
            }
        }
        for (int i = 0; i < MAPPER_DEV_COUNT; ++i)
            if (m_added[i] && m_trackers[i])
                m_trackers[i]->Tick(m_staleTimeout, m_extraLatency);
    }

    bool ShouldBlockStandbyMode() override { return false; }
    void EnterStandby() override {}
    void LeaveStandby() override {}

private:
    bool StartSocket() {
#if defined(_WIN32)
        WSADATA wsa;
        if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return false;
#endif
        m_sock = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
        if (m_sock == kBadSocket) return false;
        sockaddr_in addr{};
        addr.sin_family = AF_INET;
        addr.sin_addr.s_addr = htonl(INADDR_ANY);
        addr.sin_port = htons((uint16_t)m_port);
        if (bind(m_sock, (sockaddr*)&addr, sizeof(addr)) != 0) {
#if defined(_WIN32)
            closesocket(m_sock);
#else
            close(m_sock);
#endif
            m_sock = kBadSocket;
            return false;
        }
        // 200 ms recv timeout so the loop can notice m_run going false.
#if defined(_WIN32)
        DWORD tv = 200;
        setsockopt(m_sock, SOL_SOCKET, SO_RCVTIMEO, (const char*)&tv, sizeof(tv));
#else
        timeval tv{0, 200 * 1000};
        setsockopt(m_sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
#endif
        return true;
    }

    void RecvLoop() {
        char buf[512];
        while (m_run) {
            int n = (int)recv(m_sock, buf, sizeof(buf), 0);
            if (n != kMapperPacketSize) continue;  // timeout (n<0) or junk
            MapperPosePacket p;
            std::memcpy(&p, buf, sizeof(p));
            if (p.magic != kMapperMagic) continue;
            if (p.device >= MAPPER_DEV_COUNT) continue;
            // Mark it seen; RunFrame does the announcing. OnPacket is safe
            // before activation -- it caches the pose and its publish paths
            // already guard on a valid object id.
            m_seen[p.device].store(true, std::memory_order_relaxed);
            m_trackers[p.device]->OnPacket(p);
        }
    }

    MapperTracker* m_trackers[MAPPER_DEV_COUNT] = {};
    /// Set by the recv thread on the first packet for a role.
    std::atomic<bool> m_seen[MAPPER_DEV_COUNT] = {};
    /// Owned solely by RunFrame, so it needs no synchronisation.
    bool m_added[MAPPER_DEV_COUNT] = {};
    std::thread m_thread;
    std::atomic<bool> m_run{false};
    socket_t m_sock = kBadSocket;
    int m_port = 5170;
    double m_staleTimeout = 0.5;
    double m_extraLatency = 0.025;
};

// ---------------------------------------------------------------------------
// factory
// ---------------------------------------------------------------------------
static MapperProvider g_provider;

#if defined(_WIN32)
#define MAPPER_EXPORT extern "C" __declspec(dllexport)
#else
#define MAPPER_EXPORT extern "C" __attribute__((visibility("default")))
#endif

MAPPER_EXPORT void* HmdDriverFactory(const char* interfaceName, int* returnCode) {
    if (std::strcmp(interfaceName, IServerTrackedDeviceProvider_Version) == 0)
        return &g_provider;
    if (returnCode) *returnCode = VRInitError_Init_InterfaceNotFound;
    return nullptr;
}
