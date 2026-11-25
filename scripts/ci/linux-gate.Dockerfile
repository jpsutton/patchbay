# pb-20261124-20: local mirror of CI's `linux` job (ubuntu-24.04). Built by scripts/ci/linux_gate.py,
# which passes APT_PACKAGES parsed from scripts/ci/deps-ubuntu.sh so the two cannot drift.
FROM ubuntu:24.04
ARG APT_PACKAGES
ENV DEBIAN_FRONTEND=noninteractive
# nodejs/npm: the runner image has them; a mesh test dispatches a codex worker through npx.
# git from the git-core PPA + git-lfs installed system-wide: the GitHub runner has both, and the
# teams tests (pb-20261124-16) only fail where /etc/gitconfig carries the lfs filter.
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends ca-certificates software-properties-common gpg-agent \
      git git-lfs rsync python3 nodejs npm sudo $APT_PACKAGES \
 && add-apt-repository -y ppa:git-core/ppa \
 && apt-get update -qq && apt-get install -y -qq --no-install-recommends git \
 && git lfs install --system \
 && rm -rf /var/lib/apt/lists/*
# Build-time proof the system lfs filter is on, as on the runner.
RUN test "$(git config --system filter.lfs.required)" = true
# Rust is `stable`, what CI installs (dtolnay/rust-toolchain@stable); the repo pins no toolchain.
ENV CARGO_HOME=/cache/cargo RUSTUP_HOME=/opt/rustup PATH=/cache/cargo/bin:/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | CARGO_HOME=/opt/cargo sh -s -- -y --profile minimal --default-toolchain stable \
 && ln -s /opt/cargo/bin/* /usr/local/bin/
