# needle: election-night county model. Library (also used by the pybind11 bindings) plus the
# standalone `susq-needle` NATS service.
set(SUSQ_FIXTURES_DIR ${CMAKE_CURRENT_SOURCE_DIR}/../../contracts/fixtures)

add_library(susq_needle STATIC src/model.cpp src/service.cpp)
target_include_directories(susq_needle PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_link_libraries(susq_needle PUBLIC susq_contracts)
set_target_properties(susq_needle PROPERTIES POSITION_INDEPENDENT_CODE ON)
susq_target_warnings(susq_needle)

if(SUSQ_BUILD_TESTS)
  add_executable(needle_test
    tests/model_test.cpp
    tests/synthetic_test.cpp
    tests/service_test.cpp)
  target_link_libraries(needle_test PRIVATE susq_needle GTest::gtest_main)
  target_compile_definitions(needle_test PRIVATE SUSQ_FIXTURES_DIR="${SUSQ_FIXTURES_DIR}")
  susq_target_warnings(needle_test)
  gtest_discover_tests(needle_test)
endif()

if(SUSQ_BUILD_NEEDLE_SERVICE)
  # Stamped into every Header.producer.git_sha (resolved at configure time).
  execute_process(COMMAND git rev-parse --short=12 HEAD
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
    OUTPUT_VARIABLE SUSQ_GIT_SHA OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_QUIET)
  if(NOT SUSQ_GIT_SHA)
    set(SUSQ_GIT_SHA unknown)
  endif()

  add_executable(susq-needle src/main.cpp src/nats_bus.cpp)
  target_link_libraries(susq-needle PRIVATE susq_needle susq_cnats)
  target_compile_definitions(susq-needle PRIVATE SUSQ_GIT_SHA="${SUSQ_GIT_SHA}")
  susq_target_warnings(susq-needle)

  if(SUSQ_BUILD_TESTS)
    add_executable(needle_nats_test tests/nats_integration_test.cpp)
    target_link_libraries(needle_nats_test PRIVATE susq_contracts susq_cnats GTest::gtest_main)
    target_compile_definitions(needle_nats_test PRIVATE
      SUSQ_FIXTURES_DIR="${SUSQ_FIXTURES_DIR}"
      SUSQ_NEEDLE_BIN="$<TARGET_FILE:susq-needle>")
    add_dependencies(needle_nats_test susq-needle)
    susq_target_warnings(needle_nats_test)
    gtest_discover_tests(needle_nats_test PROPERTIES TIMEOUT 60)
  endif()
endif()
