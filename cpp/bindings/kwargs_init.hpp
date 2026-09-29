// Keyword-argument constructors for plain config structs: RaceSpec(id="x", state="OH", sd=3.0).
#pragma once

#include <pybind11/pybind11.h>

namespace susq::bindings {

namespace py = pybind11;

/// Adds `T(**kwargs)`: default-constructs T, then assigns each keyword through the bound
/// attributes (unknown names raise AttributeError, wrong types raise TypeError).
template <class T, class... Extra>
void AddKwargsInit(py::class_<T, Extra...>& cls) {
  cls.def(py::init([](const py::kwargs& kwargs) {
            T value;
            py::object handle = py::cast(&value, py::return_value_policy::reference);
            for (auto item : kwargs) handle.attr(item.first) = item.second;
            return value;
          }),
          "Construct with keyword arguments; omitted fields keep their defaults.");
}

}  // namespace susq::bindings
