# Desktop UI verify stopgap: headed Linux desktop under Xvfb. The nightly GHA
# stand-in lane runs only the stand-in image. The scheduled live Jev lane and
# local runs may install the real Desktop package.
#
# INSTALL_CLAUDE_DESKTOP=1 installs Anthropic's Linux beta package. Cowork is
# deliberately not supported: /dev/kvm is not passed by default and the host
# should have about 8 GB RAM plus 25 GB free disk for Cowork. The Xvfb path is
# for the standard UI / 3P gateway / Code and Chat surfaces.
ARG INSTALL_CLAUDE_DESKTOP=1
ARG CLAUDE_DESKTOP_VERSION=3.6556.2

FROM ubuntu:24.04
ARG INSTALL_CLAUDE_DESKTOP
ARG CLAUDE_DESKTOP_VERSION
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg \
      xvfb openbox dbus-x11 at-spi2-core x11-utils xdotool scrot procps \
      python3 python3-gi python3-pyatspi gir1.2-atspi-2.0 gir1.2-gtk-3.0 \
      fonts-dejavu-core \
      libgtk-3-0 libnotify4 libnss3 xdg-utils libatspi2.0-0 libdrm2 libgbm1 \
      libxcb-dri3-0 libsecret-1-0 libxtst6 libuuid1 libpipewire-0.3-0 libglib2.0-bin \
    && rm -rf /var/lib/apt/lists/*

RUN if [ "$INSTALL_CLAUDE_DESKTOP" = "1" ]; then \
      curl -fsSLo /usr/share/keyrings/claude-desktop-archive-keyring.asc \
        https://downloads.claude.ai/claude-desktop/key.asc \
      && echo "deb [arch=amd64,arm64 signed-by=/usr/share/keyrings/claude-desktop-archive-keyring.asc] https://downloads.claude.ai/claude-desktop/apt/stable stable main" \
        > /etc/apt/sources.list.d/claude-desktop.list \
      && apt-get update \
      && apt-get install -y --no-install-recommends "claude-desktop=$CLAUDE_DESKTOP_VERSION" \
      && rm -rf /var/lib/apt/lists/*; \
    else \
      echo "INSTALL_CLAUDE_DESKTOP=0: stand-in only"; \
    fi

RUN useradd -m -s /bin/bash claude \
    && mkdir -p /home/claude/work /opt/agent-tail-desktop /out /etc/claude-desktop \
    && chown -R claude:claude /home/claude /opt/agent-tail-desktop

COPY desktop_driver.py standin.py entrypoint.sh session.sh /opt/agent-tail-desktop/
COPY scenarios/ /opt/agent-tail-desktop/scenarios/
RUN chmod +x /opt/agent-tail-desktop/*.sh /opt/agent-tail-desktop/*.py

WORKDIR /opt/agent-tail-desktop
ENTRYPOINT ["/opt/agent-tail-desktop/entrypoint.sh"]
CMD ["standin"]
