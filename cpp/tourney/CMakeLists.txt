# tourney: competitor field / P(1st) model (SPEC §6). Library only, no I/O.
add_library(susq_tourney STATIC
  src/field.cpp
  src/evaluate.cpp
  src/fit.cpp)
target_include_directories(susq_tourney PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
# Reuses the sim work-sharing loop (header only).
target_link_libraries(susq_tourney PUBLIC susq_sim)
set_target_properties(susq_tourney PROPERTIES POSITION_INDEPENDENT_CODE ON)
susq_target_warnings(susq_tourney)

if(SUSQ_BUILD_TESTS)
  add_executable(tourney_test
    tests/analytic_test.cpp
    tests/montecarlo_test.cpp
    tests/fit_test.cpp)
  target_link_libraries(tourney_test PRIVATE susq_tourney GTest::gtest_main)
  susq_target_warnings(tourney_test)
  gtest_discover_tests(tourney_test)
endif()
