cmake_minimum_required(VERSION 3.22)
project(q1tracker C CXX)
set(CMAKE_CXX_STANDARD 17)

# Khronos publishes an official OpenXR loader AAR for Android on Maven Central
# (org.khronos.openxr:openxr_loader_for_android), exposed to CMake via prefab.
# That is far more reliable than building the SDK's CMake under the NDK, and it
# still avoids the license-gated Meta SDK -- the runtime lives on the headset.
# NB: the prefab PACKAGE is "OpenXR" (see prefab.json), not the Maven
# artifact name; modules are OpenXR::openxr_loader and OpenXR::headers.
find_package(OpenXR REQUIRED CONFIG)

add_library(native_app_glue STATIC
    ${ANDROID_NDK}/sources/android/native_app_glue/android_native_app_glue.c)
target_include_directories(native_app_glue PUBLIC
    ${ANDROID_NDK}/sources/android/native_app_glue)

add_library(questtracker SHARED quest_tracker.cpp)
target_include_directories(questtracker PRIVATE
    ${ANDROID_NDK}/sources/android/native_app_glue)

# The upstream project also streamed frames from a UVC module bolted to the
# headset, which needed libusb vendored under third_party/. Insight Map Loader reads the
# Quest's own tracking cameras from a rooted native process instead, so that
# path is compiled out -- it also never ran against real hardware upstream.
target_compile_definitions(questtracker PRIVATE Q1T_UVC=0)

# NativeActivity's entry point lives in the glue; without this the linker drops
# ANativeActivity_onCreate and the app starts then immediately dies.
target_link_options(questtracker PRIVATE "-u" "ANativeActivity_onCreate")
# 16 KB-page devices refuse a 4 KB-aligned .so outright (see android/capture).
target_link_options(questtracker PRIVATE "-Wl,-z,max-page-size=16384")
target_link_libraries(questtracker
    native_app_glue OpenXR::openxr_loader OpenXR::headers android log EGL GLESv3)
