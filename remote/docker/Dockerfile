# Android build environment for Patchbay Remote, for building the APK without a local Android
# SDK (scripts/android-build.sh). It also has what release.yml uses to sign the APK and update
# the F-Droid repository, for trying those locally. CI uses GitHub's runner image instead.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    ANDROID_HOME=/opt/android-sdk \
    NDK_VERSION=27.2.12479018 \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
ENV NDK_HOME=$ANDROID_HOME/ndk/$NDK_VERSION \
    PATH=/opt/cargo/bin:/opt/fdroid/bin:/opt/android-sdk/cmdline-tools/latest/bin:/opt/android-sdk/platform-tools:$PATH

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl unzip git build-essential pkg-config file \
      openjdk-21-jdk-headless nodejs npm python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g pnpm@10

# Tauri's Android template compiles against SDK 37 and its plugins against 36. Builds run as the
# host user, so the SDK must be writable for Gradle's own downloads.
RUN mkdir -p $ANDROID_HOME/cmdline-tools \
    && curl -fsSL -o /tmp/tools.zip https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip \
    && unzip -q /tmp/tools.zip -d $ANDROID_HOME/cmdline-tools \
    && mv $ANDROID_HOME/cmdline-tools/cmdline-tools $ANDROID_HOME/cmdline-tools/latest \
    && rm /tmp/tools.zip \
    && yes | sdkmanager --licenses >/dev/null \
    && sdkmanager "platform-tools" "platforms;android-36" "platforms;android-37.0" "build-tools;36.0.0" "ndk;$NDK_VERSION" \
    && chmod -R a+rwX $ANDROID_HOME

RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
    && rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android \
    && chmod -R a+rwX $RUSTUP_HOME $CARGO_HOME

# The same fdroidserver release.yml installs; Ubuntu's (2.2) can't read current APKs.
RUN python3 -m venv /opt/fdroid && /opt/fdroid/bin/pip install --no-cache-dir fdroidserver==2.4.5

WORKDIR /src
