cmake_minimum_required(VERSION 3.25)
project(susq_models LANGUAGES CXX VERSION 0.1.0
        DESCRIPTION "Susquatch C++20 models: sim, tourney, needle (never linked with Rust)")

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

option(SUSQ_BUILD_TESTS "Build unit tests" ON)
option(SUSQ_BUILD_NEEDLE_SERVICE "Build the needle NATS service" ON)
option(SUSQ_BUILD_PYTHON "Build pybind11 modules" OFF)

if(SUSQ_BUILD_TESTS)
  enable_testing()
endif()
include(cmake/deps.cmake)

add_subdirectory(contracts)

# Model components (feat/cpp-models): sim/, tourney/, needle/, bindings/.
foreach(component sim tourney needle bindings)
  if(EXISTS ${CMAKE_CURRENT_SOURCE_DIR}/${component}/CMakeLists.txt)
    add_subdirectory(${component})
  endif()
endforeach()
