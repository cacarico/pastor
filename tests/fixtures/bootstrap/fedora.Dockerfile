# Fedora 42: systemd and dnf by default. systemd-pam holds pam_systemd and
# dbus-broker is the bus the user manager talks over. git, curl, tar and
# ca-certificates are there already, so the prerequisites step has
# something to find.
FROM fedora:42

RUN dnf install -y \
        systemd systemd-pam dbus-daemon dbus-broker openssh-server openssh-clients \
        git curl tar ca-certificates \
    && dnf clean all

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
