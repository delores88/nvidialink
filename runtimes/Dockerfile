# Spark Dev Bridge's own runtime; never tag or modify a PhaseForge runtime.
# ARM64 base pin copied from the read-only prototype (see PROVENANCE-REMOTE.md).
FROM rust:1.98.1-slim-trixie@sha256:441452f8033b93f69dce3a3ff5c821c411f0a580723e6343932fc34a1b03ba5d
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update \
 && apt-get install -y --no-install-recommends git pkg-config libdbus-1-dev libssl-dev python3 python3-venv nodejs npm ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY requirements.txt /opt/sparkdev/requirements.txt
RUN python3 -m venv /opt/pyenv \
 && /opt/pyenv/bin/pip install --no-cache-dir --disable-pip-version-check --require-hashes --only-binary=:all: -r /opt/sparkdev/requirements.txt
ENV PATH=/opt/pyenv/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_HOME=/cargo-home CARGO_TARGET_DIR=/target PYTHONDONTWRITEBYTECODE=1
RUN mkdir -p /cargo-home /target /npm-cache /pip-cache /work \
 && chown 1000:1000 /cargo-home /target /npm-cache /pip-cache /work
WORKDIR /work
ENTRYPOINT ["/usr/bin/python3"]
