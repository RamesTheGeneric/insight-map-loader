// posed_frame.h -- MPF1, the "Mapper Posed Frame" streaming contract.
//
// A pose-known camera (this Quest, or the HMD head-cam) does not need SLAM to
// contribute to the shared map: mapping degenerates to triangulation from known
// poses (CLAUDE.md §2.1). For that the host needs, per frame, the image AND the
// 6DoF pose of the camera at the instant that frame was captured, in a single
// consistent frame. This is that pair.
//
// Transport is TCP (frames are large and must not be dropped mid-image), one
// MPF1 record per frame: a fixed 52-byte little-endian header, then `len` bytes
// of JPEG (the camera streams MJPEG, so no re-encode). Distinct from MPT1, which
// is the driver's 68-byte UDP pose datagram -- different job, different link.
//
// The pose is the HEADSET pose in the same LOCAL(+recenter-fix) frame the MPT1
// tracker poses use, so once the Quest is SpaceCalibrated the posed frames and
// the tracker live in one frame. It is NOT yet the camera pose: the rigid
// headset->camera (hand-eye) extrinsic is an unsolved calibration item
// (CLAUDE.md §7 stage 2), applied by the host, not baked in here.
//
// t_ns is the frame's CLOCK_BOOTTIME arrival stamp -- the same value used to
// look up the pose on-device, so image and pose are matched to the sensor's
// own timeline rather than to network arrival. It remains an *arrival* stamp
// (a USB camera cannot do better); the residual capture->stamp latency is a
// temporal-calibration constant (CLAUDE.md §7 stage 1), not something this
// contract can remove.

#pragma once
#include <stdint.h>

// 'M''P''F''1' little-endian. Bump the trailing digit on any layout change.
static const uint32_t kPosedFrameMagic = 0x3146504Du;

#pragma pack(push, 1)
struct PosedFrameHeader {
    uint32_t magic;      //  0  == kPosedFrameMagic
    uint64_t t_ns;       //  4  CLOCK_BOOTTIME capture (arrival) stamp
    uint32_t index;      // 12  monotonic frame counter, gaps => dropped frames
    uint8_t  pose_valid; // 16  1 = pose TRACKED at t_ns, 0 = not (image still sent)
    uint8_t  pad[3];     // 17  keep pose 4-byte aligned; send 0
    float    pose[7];    // 20  x,y,z, qw,qx,qy,qz  (headset, LOCAL+fix frame)
    uint32_t len;        // 48  JPEG byte length that follows this header
};                       // 52 total, then `len` bytes of JPEG
#pragma pack(pop)

static const int kPosedFrameHeaderSize = 52;
