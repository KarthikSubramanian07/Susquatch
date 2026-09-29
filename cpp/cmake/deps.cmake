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

# NATS C client for the needle service: `susq_cnats` (include as <nats/nats.h>).
# Prefer an installed cnats (brew install cnats); otherwise build nats.c from source with TLS off
# (the bus is loopback-only on the VM, and CI images have no cnats package).
if(SUSQ_BUILD_NEEDLE_SERVICE AND NOT TARGET susq_cnats)
  add_library(susq_cnats INTERFACE)
  find_package(Threads REQUIRED)
  # The installed cnats config links OpenSSL targets without finding them itself.
  find_package(OpenSSL QUIET)
  if(OpenSSL_FOUND)
    find_package(cnats CONFIG QUIET)
  endif()
  if(TARGET cnats::nats)
    target_link_libraries(susq_cnats INTERFACE cnats::nats)
  else()
    message(STATUS "cnats not found; building nats.c v3.14.0 from source (TLS off)")
    FetchContent_Declare(cnats
      URL https://github.com/nats-io/nats.c/archive/refs/tags/v3.14.0.tar.gz
      DOWNLOAD_EXTRACT_TIMESTAMP TRUE)
    set(NATS_BUILD_WITH_TLS OFF CACHE BOOL "" FORCE)
    set(NATS_BUILD_STREAMING OFF CACHE BOOL "" FORCE)
    set(NATS_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
    set(NATS_BUILD_LIB_SHARED OFF CACHE BOOL "" FORCE)
    FetchContent_MakeAvailable(cnats)
    # Source builds expose headers flat under src/; mirror the installed <nats/...> layout.
    set(_susq_cnats_inc ${CMAKE_BINARY_DIR}/cnats_include)
    file(COPY ${cnats_SOURCE_DIR}/src/nats.h ${cnats_SOURCE_DIR}/src/status.h
              ${cnats_SOURCE_DIR}/src/version.h DESTINATION ${_susq_cnats_inc}/nats)
    target_include_directories(susq_cnats SYSTEM INTERFACE ${_susq_cnats_inc})
    target_link_libraries(susq_cnats INTERFACE nats_static)
  endif()
endif()

# Warning flags for first-party model targets (AGENT_BRIEF: -Wall -Wextra -Wpedantic clean).
function(susq_target_warnings target)
  if(MSVC)
    target_compile_options(${target} PRIVATE /W4)
  else()
    target_compile_options(${target} PRIVATE -Wall -Wextra -Wpedantic)
  endif()
endfunction()
