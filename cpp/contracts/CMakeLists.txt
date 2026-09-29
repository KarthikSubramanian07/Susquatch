# Generated C++ types for the single schema source in /contracts/proto.
set(SUSQ_PROTO_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../../contracts/proto)
set(SUSQ_PROTOS
  ${SUSQ_PROTO_ROOT}/susq/v1/common.proto
  ${SUSQ_PROTO_ROOT}/susq/v1/targets.proto
  ${SUSQ_PROTO_ROOT}/susq/v1/bus.proto
  ${SUSQ_PROTO_ROOT}/susq/v1/ops.proto)

file(MAKE_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR}/gen)
add_library(susq_contracts STATIC ${SUSQ_PROTOS} contracts.cpp)
set_target_properties(susq_contracts PROPERTIES POSITION_INDEPENDENT_CODE ON)
protobuf_generate(TARGET susq_contracts LANGUAGE cpp
  IMPORT_DIRS ${SUSQ_PROTO_ROOT}
  PROTOC_OUT_DIR ${CMAKE_CURRENT_BINARY_DIR}/gen)
target_include_directories(susq_contracts PUBLIC
  ${CMAKE_CURRENT_BINARY_DIR}/gen ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_link_libraries(susq_contracts PUBLIC protobuf::libprotobuf)

if(SUSQ_BUILD_TESTS)
  add_executable(contracts_golden_test tests/golden_test.cpp)
  target_link_libraries(contracts_golden_test PRIVATE susq_contracts GTest::gtest_main)
  target_compile_definitions(contracts_golden_test PRIVATE
    SUSQ_FIXTURES_DIR="${CMAKE_CURRENT_SOURCE_DIR}/../../contracts/fixtures")
  gtest_discover_tests(contracts_golden_test)
endif()
