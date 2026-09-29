# Shared third-party setup for every C++ component.
include(FetchContent)

find_package(Protobuf CONFIG QUIET)
if(NOT Protobuf_FOUND)
  find_package(Protobuf REQUIRED)  # distro packages ship the Find module instead of a config
endif()

if(SUSQ_BUILD_TESTS AND NOT TARGET GTest::gtest_main)
  find_package(GTest CONFIG QUIET)
  if(NOT GTest_FOUND)
    FetchContent_Declare(googletest
      URL https://github.com/google/googletest/archive/refs/tags/v1.17.0.tar.gz
      DOWNLOAD_EXTRACT_TIMESTAMP TRUE)
    set(INSTALL_GTEST OFF CACHE BOOL "" FORCE)
    FetchContent_MakeAvailable(googletest)
  endif()
  include(GoogleTest)
endif()
