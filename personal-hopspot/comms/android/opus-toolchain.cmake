# The voice codec is built with CMake. The NDK toolchain file defaults to
# 32-bit ARM unless the ABI is set before it is included, and the Mac
# archiver cannot pack the phone's object files. This wrapper pins the
# phone before handing off to the NDK.
set(ANDROID_ABI arm64-v8a CACHE STRING "" FORCE)
set(ANDROID_PLATFORM android-30 CACHE STRING "" FORCE)
if(NOT DEFINED ENV{ANDROID_NDK_HOME})
  message(FATAL_ERROR "ANDROID_NDK_HOME is required to build the voice codec")
endif()
include($ENV{ANDROID_NDK_HOME}/build/cmake/android.toolchain.cmake)
