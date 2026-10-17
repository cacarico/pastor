# fedora:42: systemd and dnf are the default; a supported release at the
# time of writing (40 is end of life). systemd-pam holds pam_systemd, which
# gives an ssh login its systemd user session and XDG_RUNTIME_DIR, which
# user mode's services step needs for `systemctl --user`; dbus-broker is
# the bus that user session's manager talks over (Fedora's default bus,
# which the container image leaves out); util-linux has the
# runuser the tests check services with. git, curl, tar and ca-certificates
# are installed here (not left for bootstrap) so push_user_<distro> (no
# root, so it cannot install anything) has something to find already there;
# push_rerun_<distro> removes git by hand before it runs bootstrap, to
# exercise the install path push_root_<distro> would otherwise skip over.
FROM fedora:42

RUN dnf install -y \
        systemd systemd-pam dbus-daemon dbus-broker openssh-server openssh-clients sudo \
        util-linux \
        git curl tar ca-certificates \
    && dnf clean all

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
         echo 'UsePAM yes'; \
       } >> /etc/ssh/sshd_config \
    && systemctl enable sshd

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
