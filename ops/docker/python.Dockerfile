# syntax=docker/dockerfile:1.7
# Python services: bridge, python-scheduler, status-pusher, kill-poller, watchdog.
# Built natively on the aarch64 server (and on Apple Silicon for the Mac fallback). No cross-compiling.
#
#   docker build -f ops/docker/python.Dockerfile -t susquatch/python .

ARG PYTHON_IMAGE=python:3.13-slim-trixie
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.9

FROM ${UV_IMAGE} AS uv

# ── pybind11 models (sim, tourney, needle replays) for the scheduler, if cpp/bindings exists ──────────────
FROM ${PYTHON_IMAGE} AS models
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake ninja-build libprotobuf-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir "pybind11>=2.13"
WORKDIR /src
COPY cpp cpp
COPY contracts/proto contracts/proto
RUN mkdir -p /out/modules /out/lib \
 && if [ -f cpp/bindings/CMakeLists.txt ]; then \
      cmake -S cpp -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
        -DSUSQ_BUILD_PYTHON=ON -DSUSQ_BUILD_TESTS=OFF -DSUSQ_BUILD_NEEDLE_SERVICE=OFF \
        -Dpybind11_DIR="$(python -m pybind11 --cmakedir)" \
      && cmake --build build \
      && find build -name '*.cpython-313*.so' -exec cp {} /out/modules/ \; \
      && for so in /out/modules/*.so; do \
           ldd "$so" | awk '/=> \/usr\/lib/ && /protobuf|absl|utf8/ {print $3}' | xargs -r cp -Lt /out/lib/; \
         done; \
    else echo "cpp/bindings not present yet: scheduler image ships without native models"; fi

# ── dependencies + project into /app/.venv ────────────────────────────────────────────────────────────────
FROM ${PYTHON_IMAGE} AS build
COPY --from=uv /uv /usr/local/bin/uv
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never UV_PROJECT_ENVIRONMENT=/app/.venv
WORKDIR /app
COPY pyproject.toml uv.lock README.md LICENSE ./
RUN --mount=type=cache,target=/root/.cache/uv uv sync --locked --no-dev --no-install-project
COPY py py
COPY bridge_py bridge_py
# uv caches built wheels of the local project keyed on pyproject.toml, not the sources: always rebuild ours.
RUN --mount=type=cache,target=/root/.cache/uv uv sync --locked --no-dev --no-editable --reinstall-package susquatch

# ── runtime ───────────────────────────────────────────────────────────────────────────────────────────────
FROM ${PYTHON_IMAGE}
ARG GIT_SHA=unknown
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 \
    PATH=/app/.venv/bin:$PATH LD_LIBRARY_PATH=/opt/susq/lib \
    SUSQ_HOME=/susq SUSQ_GIT_SHA=${GIT_SHA} TZ=UTC
RUN groupadd --system --gid 10001 susq && useradd --system --uid 10001 --gid susq --home /susq susq
COPY --from=build /app/.venv /app/.venv
COPY --from=models /out/lib /opt/susq/lib
COPY --from=models /out/modules /app/.venv/lib/python3.13/site-packages/
WORKDIR /susq
USER susq
CMD ["python", "-m", "susq.ops.status_push"]
