// Copying std::vector data into fresh NumPy arrays (the model keeps ownership of its buffers,
// Python gets independent arrays that stay valid after the model is re-run or destroyed).
#pragma once

#include <pybind11/numpy.h>
#include <pybind11/pybind11.h>

#include <algorithm>
#include <cstddef>
#include <vector>

namespace susq::bindings {

namespace py = pybind11;

template <class T>
py::array_t<T> ToArray(const std::vector<T>& v) {
  py::array_t<T> out(static_cast<py::ssize_t>(v.size()));
  std::copy(v.begin(), v.end(), out.mutable_data());
  return out;
}

template <class T>
py::array_t<T> ToArray2D(const std::vector<T>& v, std::size_t rows, std::size_t cols) {
  py::array_t<T> out({static_cast<py::ssize_t>(rows), static_cast<py::ssize_t>(cols)});
  std::copy(v.begin(), v.begin() + static_cast<std::ptrdiff_t>(rows * cols), out.mutable_data());
  return out;
}

void BindSim(py::module_& m);
void BindTourney(py::module_& m);
void BindNeedle(py::module_& m);

}  // namespace susq::bindings
