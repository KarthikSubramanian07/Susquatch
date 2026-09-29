# syntax=docker/dockerfile:1.7
# needle: the election-night county model as its own NATS service. Built natively on aarch64.
# (sim/tourney are pybind11 modules and ship inside the Python image instead.)
#
#   docker build -f ops/docker/cpp.Dockerfile -t susquatch/needle .

ARG CNATS_VERSION=v3.14.0

FROM debian:trixie AS build
ARG CNATS_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake ninja-build git ca-certificates libssl-dev \
      libprotobuf-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
# cnats (NATS C client) isn't packaged by Debian; build the pinned release.
RUN git clone --depth 1 --branch "${CNATS_VERSION}" https://github.com/nats-io/nats.c.git /tmp/cnats \
 && cmake -S /tmp/cnats -B /tmp/cnats/build -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DNATS_BUILD_STREAMING=OFF -DNATS_BUILD_EXAMPLES=OFF -DBUILD_TESTING=OFF \
 && cmake --build /tmp/cnats/build && cmake --install /tmp/cnats/build \
 && rm -rf /tmp/cnats
WORKDIR /src
COPY cpp cpp
COPY contracts/proto contracts/proto
ARG NEEDLE_BIN=susq-needle
RUN test -f cpp/needle/CMakeLists.txt || { echo "cpp/needle not merged yet: nothing to build" >&2; exit 1; } \
 && cmake -S cpp -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DSUSQ_BUILD_NEEDLE_SERVICE=ON -DSUSQ_BUILD_TESTS=OFF -DSUSQ_BUILD_PYTHON=OFF \
 && cmake --build build --target "${NEEDLE_BIN}" \
 && mkdir -p /out/bin /out/lib \
 && cp "$(find build -type f -name "${NEEDLE_BIN}" -perm -u+x | head -n1)" /out/bin/needle \
 && ldd /out/bin/needle | awk '/=> \// && !/libc\.so|libm\.so|libgcc_s|libstdc\+\+|ld-linux/ {print $3}' \
      | xargs -r cp -Lt /out/lib/

FROM debian:trixie-slim
ARG GIT_SHA=unknown
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 susq && useradd --system --uid 10001 --gid susq --home /susq susq
COPY --from=build /out/lib /opt/susq/lib
COPY --from=build /out/bin/needle /usr/local/bin/needle
ENV LD_LIBRARY_PATH=/opt/susq/lib SUSQ_HOME=/susq SUSQ_GIT_SHA=${GIT_SHA} TZ=UTC
WORKDIR /susq
USER susq
ENTRYPOINT ["needle"]
