# Arch Linux: a rolling release, so "latest" is the only tag there is. Its
# systemd package carries pam_systemd. git, curl, tar and ca-certificates
# are there already, so the prerequisites step has something to find.
FROM archlinux:latest

# A mirror that fails to serve a file fails pacman for good, so it is tried
# again.
RUN for i in 1 2 3 4; do \
        pacman -Syu --noconfirm \
        && pacman -S --noconfirm --needed systemd dbus openssh git curl tar ca-certificates \
        && break; \
        test "$i" != 4 || exit 1; \
    done \
    && pacman -Scc --noconfirm

# dbus-broker, Arch's bus, cannot reach systemd's activation socket in a
# rootless container; the reference dbus-daemon runs the same unit name.
RUN { \
        echo '[Unit]'; echo 'Description=D-Bus System Message Bus'; echo 'Requires=dbus.socket'; \
        echo '[Service]'; echo 'Type=notify'; echo 'ExecStart=/usr/bin/dbus-daemon --system --address=systemd: --nofork --nopidfile --systemd-activation --syslog-only'; \
        echo 'ExecReload=/usr/bin/dbus-send --print-reply --system --type=method_call --dest=org.freedesktop.DBus / org.freedesktop.DBus.ReloadConfig'; \
    } > /etc/systemd/system/dbus.service

# systemd-userdbd cannot read /etc/passwd's users from its sandbox in a
# rootless container, and then systemd cannot resolve `me` for its user
# manager; without the daemon, plain NSS answers.
RUN systemctl mask systemd-userdbd.socket systemd-userdbd.service

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
    && systemctl enable sshd

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
