# syntax=docker/dockerfile:1.7
# Python services: bridge, python-scheduler, status-pusher, kill-poller, watchdog.
# Includes the C++ models (susq_models: sim, tourney, needle) through the `models` dependency group, so the
# research facade uses the C++ engine rather than the numpy reference. Built natively on aarch64 (and on
# Apple Silicon for the Mac fallback). No cross-compiling.
#
#   docker build -f ops/docker/python.Dockerfile -t susquatch/python .

ARG PYTHON_IMAGE=python:3.13-slim-trixie
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.9

FROM ${UV_IMAGE} AS uv

# ── dependencies + C++ models + project into /app/.venv ───────────────────────────────────────────────────
FROM ${PYTHON_IMAGE} AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake ninja-build git libprotobuf-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
COPY --from=uv /uv /usr/local/bin/uv
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never UV_PROJECT_ENVIRONMENT=/app/.venv
WORKDIR /app
COPY pyproject.toml uv.lock README.md LICENSE ./
# susq-models is a path dependency on cpp/ (scikit-build-core + pybind11); it needs the shared schema too.
COPY cpp cpp
COPY contracts/proto contracts/proto
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-dev --group models --no-install-project
COPY py py
COPY bridge_py bridge_py
# uv caches built wheels of the local project keyed on pyproject.toml, not the sources: always rebuild ours.
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-dev --group models --no-editable --reinstall-package susquatch
# The extension links the distro's protobuf (and its abseil/utf8 deps); carry exactly those to the runtime.
RUN mkdir -p /out/lib \
 && for so in $(find /app/.venv -name 'susq_models*.so'); do \
      ldd "$so" | awk '/=> \// && /protobuf|absl|utf8/ {print $3}' | xargs -r cp -Lt /out/lib/; \
    done \
 && ls /out/lib | grep -q libprotobuf \
 && /app/.venv/bin/python -c "import susq_models; print('susq_models ok')"

# ── runtime ───────────────────────────────────────────────────────────────────────────────────────────────
FROM ${PYTHON_IMAGE}
ARG GIT_SHA=unknown
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 \
    PATH=/app/.venv/bin:$PATH LD_LIBRARY_PATH=/opt/susq/lib \
    SUSQ_HOME=/susq SUSQ_GIT_SHA=${GIT_SHA} TZ=UTC
RUN groupadd --system --gid 10001 susq && useradd --system --uid 10001 --gid susq --home /susq susq
COPY --from=build /app/.venv /app/.venv
COPY --from=build /out/lib /opt/susq/lib
WORKDIR /susq
USER susq
CMD ["python", "-m", "susq.ops.status_push"]
