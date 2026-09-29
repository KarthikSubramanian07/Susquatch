# pybind11 module `susq_models` (sim, tourney, needle). Built by scikit-build-core from
# cpp/pyproject.toml, or directly with -DSUSQ_BUILD_PYTHON=ON.
if(NOT SUSQ_BUILD_PYTHON)
  return()
endif()

find_package(Python 3.13 REQUIRED COMPONENTS Interpreter Development.Module)
find_package(pybind11 CONFIG REQUIRED)

pybind11_add_module(susq_models
  module.cpp
  sim_bindings.cpp
  tourney_bindings.cpp
  needle_bindings.cpp)
target_link_libraries(susq_models PRIVATE susq_sim susq_tourney susq_needle)
susq_target_warnings(susq_models)

install(TARGETS susq_models LIBRARY DESTINATION .)
