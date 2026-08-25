// mapper_protocol.h -- wire contract between the pose producer and this driver.
//
// In the per-tracker mono-inertial design (CLAUDE.md §0) each tracker computes
// its own filtered pose on-device and streams it to the PC over UDP. This driver
// receives those packets and republishes them to SteamVR as GenericTrackers.
//
// One datagram = one MapperPosePacket = one tracker's latest pose. The producer
// must send the pose ALREADY expressed in the SteamVR world frame
// (right-handed, y-up, metres, -z forward): the map->universe SE(3) alignment
// is applied upstream, not here, so the driver stays a dumb, low-latency relay.
//
// Little-endian. Fixed 68-byte layout, versioned by the magic value so the
// driver can reject a mismatched producer instead of misreading floats.

#pragma once
#include <stdint.h>

// 'M''P''T''1' little-endian. Bump the trailing digit on any layout change.
static const uint32_t kMapperMagic = 0x3154504Du;

// The device id IS the SteamVR role: kDevices[] in driver_mapper.cpp is indexed
// by it, so one id means one serial means one FBT role, permanently.
//
// APPEND ONLY. SteamVR keys pairings, role bindings and room calibration off
// the serial derived from this id; renumbering 0/1/2 makes it forget every
// tracker the user has ever set up. Keep in step with Device in
// insight-prime-core/src/mpt1.rs.
enum MapperDeviceId {
    MAPPER_DEV_WAIST          = 0,
    MAPPER_DEV_LEFT_FOOT      = 1,
    MAPPER_DEV_RIGHT_FOOT     = 2,
    MAPPER_DEV_CHEST          = 3,
    MAPPER_DEV_LEFT_KNEE      = 4,
    MAPPER_DEV_RIGHT_KNEE     = 5,
    MAPPER_DEV_LEFT_ELBOW     = 6,
    MAPPER_DEV_RIGHT_ELBOW    = 7,
    MAPPER_DEV_LEFT_SHOULDER  = 8,
    MAPPER_DEV_RIGHT_SHOULDER = 9,
    MAPPER_DEV_CAMERA         = 10,
    MAPPER_DEV_COUNT          = 11,
};

#pragma pack(push, 1)
struct MapperPosePacket {
    uint32_t magic;       //  0  == kMapperMagic
    uint8_t  device;      //  4  MapperDeviceId
    uint8_t  valid;       //  5  1 = tracking OK, 0 = running-but-lost
    // Battery reporting reuses the two formerly-reserved bytes, so the packet
    // stays 68 bytes and older producers (which sent 0 here) simply report
    // "unknown" rather than becoming unparseable.
    uint8_t  battery_pct; //  6  0 = not reported, else 1..100
    uint8_t  battery_flags; // 7  bit0: charging
    // The instant this pose is FOR, in the PRODUCER's clock. Not informational:
    // the driver estimates the producer<->host clock offset from these and ages
    // the pose against it, so transport jitter stops leaking into
    // poseTimeOffset. 0 means "not provided" and the driver falls back to
    // arrival time.
    uint64_t t_ns;        //  8  pose timestamp, producer clock
    float    pose[7];     // 16  x,y,z, qw,qx,qy,qz  (SteamVR world frame)
    float    vel[3];      // 44  linear velocity m/s, world frame
    float    angvel[3];   // 56  angular velocity rad/s, world frame
};                        // 68 total
#pragma pack(pop)

static const int kMapperPacketSize = 68;
