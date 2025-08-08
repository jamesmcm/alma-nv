FROM archlinux:latest AS builder

RUN pacman -Syu --noconfirm && \
  pacman -S --needed --noconfirm base-devel rust git

WORKDIR /src
COPY . .
RUN cargo build --release

FROM archlinux:latest

RUN pacman -Syu --noconfirm && \
  pacman -S --needed --noconfirm \
  gptfdisk \
  parted \
  arch-install-scripts \
  dosfstools \
  util-linux \
  cryptsetup \
  e2fsprogs && \
  pacman -Scc --noconfirm

RUN sed -i 's/#Color/Color/' /etc/pacman.conf

COPY --from=builder /src/target/release/alma /usr/local/bin/alma

WORKDIR /work
VOLUME ["/work"]

COPY docker-entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
CMD ["alma", "--help"]
