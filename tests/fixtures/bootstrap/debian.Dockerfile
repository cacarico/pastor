# debian:12 (bookworm): the current stable release. systemd as PID 1 needs
# systemd-sysv for the /sbin/init symlink, which plain `systemd` does not
# provide on Debian. git, curl, tar and ca-certificates are installed here
# (not left for bootstrap) so push_user_<distro> (no root, so it cannot
# install anything) has something to find already there; push_rerun_<distro>
# removes git by hand before it runs bootstrap, to exercise the install path
# push_root_<distro> would otherwise skip over.
FROM debian:12

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        systemd systemd-sysv dbus openssh-server sudo \
        git curl tar ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash me \
    && printf 'me ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/me \
    && chmod 0440 /etc/sudoers.d/me \
    && mkdir -p /home/me/.ssh \
    && touch /home/me/.ssh/authorized_keys \
    && chown -R me:me /home/me/.ssh \
    && chmod 700 /home/me/.ssh \
    && chmod 600 /home/me/.ssh/authorized_keys

# A fixed, non-privileged port: the test runs one container at a time
# (--network host, --test-threads 1), so nothing else binds it meanwhile.
RUN ssh-keygen -A \
    && mkdir -p /run/sshd \
    && { \
         echo 'Port 2222'; \
         echo 'PasswordAuthentication no'; \
         echo 'PermitRootLogin no'; \
         echo 'AuthorizedKeysFile .ssh/authorized_keys'; \
       } >> /etc/ssh/sshd_config \
    && systemctl enable ssh

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
