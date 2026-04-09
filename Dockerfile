FROM rust:latest

RUN apt-get update && apt-get install -y --no-install-recommends \
    libvirglrenderer-dev \
    libepoxy-dev \
    libdrm-dev \
    libpipewire-0.3-dev \
    libclang-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
