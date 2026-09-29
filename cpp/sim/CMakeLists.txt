# sim: correlated race Monte Carlo (SPEC §5). Library only, no I/O.
find_package(Threads REQUIRED)

add_library(susq_sim STATIC
  src/config.cpp
  src/generate.cpp
  src/calibrate.cpp
  src/evaluate.cpp
  src/bundles.cpp)
target_include_directories(susq_sim PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_link_libraries(susq_sim PUBLIC Threads::Threads)
set_target_properties(susq_sim PROPERTIES POSITION_INDEPENDENT_CODE ON)
susq_target_warnings(susq_sim)

add_executable(sim_bench bench/sim_bench.cpp)
target_link_libraries(sim_bench PRIVATE susq_sim)
susq_target_warnings(sim_bench)

if(SUSQ_BUILD_TESTS)
  add_executable(sim_test
    tests/rng_test.cpp
    tests/moments_test.cpp
    tests/calibration_test.cpp
    tests/chamber_test.cpp
    tests/bundles_test.cpp)
  target_link_libraries(sim_test PRIVATE susq_sim GTest::gtest_main)
  susq_target_warnings(sim_test)
  gtest_discover_tests(sim_test)
endif()
