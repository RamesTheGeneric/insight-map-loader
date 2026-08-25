cmake_minimum_required(VERSION 3.15)
project(driver_mapper LANGUAGES CXX)

# SteamVR loads a driver as a shared library exporting HmdDriverFactory. We link
# NOTHING from OpenVR -- only its driver header is needed to compile. So there is
# no openvr_api.lib dependency and no SDK build step; the header is vendored.

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

# Output name SteamVR expects: driver_<name>.dll / .so / .dylib
add_library(driver_mapper SHARED
    src/driver_mapper.cpp
)

target_include_directories(driver_mapper PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/vendor/openvr/headers
    ${CMAKE_CURRENT_SOURCE_DIR}/src
)

# SteamVR looks for the DLL under bin/<platform>/. Pick the platform dir the
# same way the manifest install layout does.
if(WIN32)
    set(MAPPER_PLATFORM win64)
    target_link_libraries(driver_mapper PRIVATE ws2_32)
    # MSVC: no lib prefix -> "driver_mapper.dll" exactly.
elseif(APPLE)
    set(MAPPER_PLATFORM osx64)
    set_target_properties(driver_mapper PROPERTIES PREFIX "" SUFFIX ".dylib")
else()
    set(MAPPER_PLATFORM linux64)
    set_target_properties(driver_mapper PROPERTIES PREFIX "")  # "driver_mapper.so"
endif()

# Stage the whole installable driver tree under build/mapper so it can be copied
# straight into <SteamVR>/drivers/ (or registered with vrpathreg).
set(MAPPER_STAGE ${CMAKE_BINARY_DIR}/mapper)
add_custom_command(TARGET driver_mapper POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E make_directory ${MAPPER_STAGE}/bin/${MAPPER_PLATFORM}
    COMMAND ${CMAKE_COMMAND} -E copy_directory
            ${CMAKE_CURRENT_SOURCE_DIR}/mapper ${MAPPER_STAGE}
    COMMAND ${CMAKE_COMMAND} -E copy
            $<TARGET_FILE:driver_mapper>
            ${MAPPER_STAGE}/bin/${MAPPER_PLATFORM}/
    COMMENT "Staging installable driver at ${MAPPER_STAGE}"
)
