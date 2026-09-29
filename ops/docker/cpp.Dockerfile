# syntax=docker/dockerfile:1.7
# needle: the election-night county model as its own NATS service (`susq-needle`). Built natively on aarch64.
# (sim/tourney/needle for Python ship inside the Python image as the susq_models extension.)
#
#   docker build -f ops/docker/cpp.Dockerfile -t susquatch/needle .
#
# cnats is resolved by cpp/cmake/deps.cmake (built from source, TLS off: the bus never leaves the host).

FROM debian:trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake ninja-build git ca-certificates libprotobuf-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY cpp cpp
COPY contracts/proto contracts/proto
RUN cmake -S cpp -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DSUSQ_BUILD_NEEDLE_SERVICE=ON -DSUSQ_BUILD_TESTS=OFF -DSUSQ_BUILD_PYTHON=OFF \
 && cmake --build build --target susq-needle \
 && mkdir -p /out/bin /out/lib \
 && cp "$(find build -type f -name susq-needle -perm -u+x | head -n1)" /out/bin/susq-needle \
 && ldd /out/bin/susq-needle | awk '/=> \// && !/libc\.so|libm\.so|libgcc_s|libstdc\+\+|ld-linux/ {print $3}' \
      | xargs -r cp -Lt /out/lib/

FROM debian:trixie-slim
ARG GIT_SHA=unknown
RUN groupadd --system --gid 10001 susq && useradd --system --uid 10001 --gid susq --home /susq susq
COPY --from=build /out/lib /opt/susq/lib
COPY --from=build /out/bin/susq-needle /usr/local/bin/susq-needle
ENV LD_LIBRARY_PATH=/opt/susq/lib SUSQ_HOME=/susq SUSQ_GIT_SHA=${GIT_SHA} TZ=UTC
WORKDIR /susq
USER susq
ENTRYPOINT ["susq-needle"]
