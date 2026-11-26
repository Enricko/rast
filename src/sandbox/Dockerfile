# The image of Terminal sandboxes (src/sandbox). Built on the server the
# first time a sandbox is needed and tagged with a hash of this file, so a
# changed file means a new image.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash-completion ca-certificates curl file git htop jq less make nano \
      openssh-client procps python3 python3-pip python3-venv rsync sqlite3 \
      tmux tree unzip vim-tiny wget xz-utils zip \
 && rm -rf /var/lib/apt/lists/* \
 && (userdel -r ubuntu 2>/dev/null || true)
RUN printf '%s\n' \
      '# RAST Terminal sandbox: App Center runtimes first, a coloured prompt.' \
      'if [ -d "${RAST_PACKAGES_BIN:-}" ]; then PATH="$RAST_PACKAGES_BIN:$PATH"; export PATH; fi' \
      'if [ -n "${BASH_VERSION:-}" ] && [ -n "${PS1:-}" ]; then' \
      '  PS1='"'"'\[\e[1;32m\]\u@rast\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '"'"'' \
      'fi' > /etc/profile.d/zz-rast.sh
CMD ["sleep", "infinity"]
