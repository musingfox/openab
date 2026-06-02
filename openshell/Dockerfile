FROM ubuntu:24.04

RUN groupadd -g 1000660000 sandbox && \
    useradd -u 1000660000 -g sandbox -m sandbox

RUN apt-get update && apt-get install -y \
    curl git ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Download OAB + native agent
ARG OAB_VERSION=openab-0.8.4-beta.10
RUN curl -L "https://github.com/openabdev/openab/releases/download/${OAB_VERSION}/${OAB_VERSION}-linux-x64.tar.gz" | \
    tar xz -C /usr/local/bin/ && \
    curl -L "https://github.com/openabdev/openab/releases/download/${OAB_VERSION}/${OAB_VERSION}-native-linux-x64.tar.gz" | \
    tar xz -C /usr/local/bin/

USER sandbox
WORKDIR /home/sandbox
