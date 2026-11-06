# Debian 13 (trixie), whose glibc the test's pastor is built against. systemd
# as PID 1 needs systemd-sysv; libpam-systemd and dbus-user-session give an
# ssh login its user session and XDG_RUNTIME_DIR, which `systemctl --user`
# needs. git, curl, tar and ca-certificates are there already, so the
# prerequisites step has something to find.
FROM debian:13

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus openssh-server \
        libpam-systemd dbus-user-session \
        git curl tar ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# User `me` is the login user bootstrap runs as. No sudo: what only root could
# do must fail the test. Lingering is on (the file systemd-logind keeps), as
# the machine's owner would have turned it on.
RUN useradd -m -s /bin/bash me \
    && mkdir -p /home/me/.ssh /var/lib/systemd/linger \
    && touch /home/me/.ssh/authorized_keys /var/lib/systemd/linger/me \
    && chown -R me:me /home/me/.ssh \
    && chmod 700 /home/me/.ssh \
    && chmod 600 /home/me/.ssh/authorized_keys

RUN ssh-keygen -A \
    && mkdir -p /run/sshd \
    && { \
         echo 'Port 2222'; \
         echo 'PasswordAuthentication no'; \
         echo 'PermitRootLogin no'; \
         echo 'AuthorizedKeysFile .ssh/authorized_keys'; \
         echo 'UsePAM yes'; \
       } >> /etc/ssh/sshd_config \
    && systemctl enable ssh

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
