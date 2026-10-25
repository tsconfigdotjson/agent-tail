# Desktop UI verify stopgap: headed Linux desktop under Xvfb. The nightly GHA
# stand-in lanes run only the stand-in image. The scheduled live Jev lane and
# local runs may install the real Desktop package; the manual Cursor probe may
# install Cursor.
#
# INSTALL_CLAUDE_DESKTOP=1 installs Anthropic's Linux beta package. Cowork is
# deliberately not supported: /dev/kvm is not passed by default and the host
# should have about 8 GB RAM plus 25 GB free disk for Cowork. The Xvfb path is
# for the standard UI / 3P gateway / Code and Chat surfaces.
#
# INSTALL_CURSOR_DESKTOP=1 installs Cursor's Linux .deb for the manual launch
# probe only. It is never needed by the Cursor stand-in.
ARG INSTALL_CLAUDE_DESKTOP=1
ARG CLAUDE_DESKTOP_VERSION=3.6556.2
ARG INSTALL_CURSOR_DESKTOP=0
ARG CURSOR_DESKTOP_VERSION=3.27.12
ARG CURSOR_DESKTOP_COMMIT=df132ffba90dfc15a12e6b8f020ba4e5d026c4be
ARG CURSOR_DESKTOP_DEB_URL=

FROM ubuntu:24.04
ARG INSTALL_CLAUDE_DESKTOP
ARG CLAUDE_DESKTOP_VERSION
ARG INSTALL_CURSOR_DESKTOP
ARG CURSOR_DESKTOP_VERSION
ARG CURSOR_DESKTOP_COMMIT
ARG CURSOR_DESKTOP_DEB_URL
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

RUN if [ "$INSTALL_CURSOR_DESKTOP" = "1" ]; then \
      deb_url="$CURSOR_DESKTOP_DEB_URL"; \
      if [ -z "$deb_url" ]; then \
        deb_url="https://downloads.cursor.com/production/${CURSOR_DESKTOP_COMMIT}/linux/x64/deb/amd64/deb/cursor_${CURSOR_DESKTOP_VERSION}_amd64.deb"; \
      fi; \
      if ! curl -fsSLo /tmp/cursor.deb "$deb_url"; then \
        deb_url=$(python3 -c 'import json, urllib.request; print(json.load(urllib.request.urlopen("https://www.cursor.com/api/download?platform=linux-x64&releaseTrack=stable"))["debUrl"])'); \
        curl -fsSLo /tmp/cursor.deb "$deb_url"; \
      fi; \
      apt-get update \
      && apt-get install -y --no-install-recommends /tmp/cursor.deb \
      && rm -f /tmp/cursor.deb \
      && rm -rf /var/lib/apt/lists/*; \
    else \
      echo "INSTALL_CURSOR_DESKTOP=0: Cursor stand-in only"; \
    fi

RUN useradd -m -s /bin/bash claude \
    && mkdir -p /home/claude/work /opt/agent-tail-desktop /out /etc/claude-desktop \
    && chown -R claude:claude /home/claude /opt/agent-tail-desktop

COPY desktop_driver.py standin.py cursor_standin.py entrypoint.sh session.sh /opt/agent-tail-desktop/
COPY scenarios/ /opt/agent-tail-desktop/scenarios/
RUN chmod +x /opt/agent-tail-desktop/*.sh /opt/agent-tail-desktop/*.py

WORKDIR /opt/agent-tail-desktop
ENTRYPOINT ["/opt/agent-tail-desktop/entrypoint.sh"]
CMD ["standin"]
