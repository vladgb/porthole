# Minimal xrdp test server for rdesk integration tests (TLS security; xrdp does not offer NLA).
# docker build -t rdesk-xrdp tests/xrdp && docker run --rm -d -p 13389:3389 --name rdesk-xrdp rdesk-xrdp
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends xrdp xorgxrdp openbox xterm dbus-x11 ssl-cert \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -s /bin/bash rdesk && echo 'rdesk:rdesk' | chpasswd \
 && echo 'exec openbox-session' > /home/rdesk/.xsession \
 && mkdir -p /home/rdesk/.config/openbox \
 && echo 'xterm -geometry 100x30+100+100 -e "echo rdesk xrdp test; exec bash" &' > /home/rdesk/.config/openbox/autostart \
 && chown -R rdesk:rdesk /home/rdesk \
 && adduser xrdp ssl-cert
EXPOSE 3389
CMD ["sh", "-c", "xrdp-sesman && exec xrdp --nodaemon"]
